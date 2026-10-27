import CmuxNextCompat
@testable import CmuxNextDaemon
import Foundation
import Testing

/// `SessionJournalRead` as a stream on a shared connection (p05-ipc-100): it opens no socket, its lines go to
/// its own sink and never count as control-sequence events, a request on the same connection is answered while
/// it is open, and a read that stops early cancels its stream.
@Suite(.serialized, .timeLimit(.minutes(1))) struct SessionJournalReadTests {
    typealias FakeDaemon = ReplyEventOrderTests.FakeDaemon

    /// A transport on `daemon` that counts the events it routes.
    private static func connect(_ daemon: FakeDaemon, events: ReplyEventOrderTests.Routed) throws -> LineTransport {
        let transport = try LineTransport(path: daemon.path, role: .control, stats: nil)
        try daemon.accept()
        transport.start(onEvent: { name, _, index in events.record(name, index) }, onClose: { _ in })
        return transport
    }

    private static func read(on transport: LineTransport, limit: Int = 20_000) -> Task<SessionJournalRead.Result, any Error> {
        Task { try await SessionJournalRead(carrier: transport).read(kinds: ["agent.session"], cursor: nil, limit: limit, deadline: .seconds(20)) }
    }

    /// The open request's id and stream id, after checking it is the bounded journal read.
    private static func readOpen(_ daemon: FakeDaemon) throws -> (id: String, stream: String) {
        let open = try daemon.readRequest()
        #expect(open["operation"] as? String == "session.journal.subscribe")
        let params = try #require(open["params"] as? [String: Any])
        #expect(params["follow"] as? Bool == false)
        #expect(params["start"] as? String == "beginning")
        return (try #require(open["id"] as? String), try #require(params["stream_id"] as? String))
    }

    private static func ack(_ id: String, _ stream: String) -> String {
        #"{"protocol":"cmux.protocol/2","type":"response","id":"\#(id)","ok":true,"result":{"stream_id":"\#(stream)","cursor":{"generation":"gen-1","revision":"0"}}}"#
    }

    private static func item(_ stream: String, _ sequence: Int) -> String {
        #"{"protocol":"cmux.protocol/2","type":"stream_item","stream_id":"\#(stream)","cursor":{"generation":"gen-1","revision":"\#(sequence)"},"item":{"kind":"agent.session","n":\#(sequence)}}"#
    }

    private static func end(_ stream: String, _ reason: String) -> String {
        #"{"protocol":"cmux.protocol/2","type":"stream_end","stream_id":"\#(stream)","reason":"\#(reason)","cursor":{"generation":"gen-1","revision":"9"}}"#
    }

    private static func routes(_ transport: LineTransport) -> Int {
        transport.state.withLock { $0.streamSinks.count }
    }

    @Test func aReadCollectsItsRecordsOffTheControlSequence() async throws {
        let daemon = try FakeDaemon()
        let events = ReplyEventOrderTests.Routed()
        let transport = try Self.connect(daemon, events: events)
        defer { transport.close() }

        let read = Self.read(on: transport)
        let open = try Self.readOpen(daemon)
        daemon.write([Self.ack(open.id, open.stream), Self.item(open.stream, 3), Self.item(open.stream, 4),
                      Self.end(open.stream, "completed")])
        let result = try await read.value

        #expect(result.records.count == 2)
        let first = try #require(result.records.first)
        let record = try #require(try JSONSerialization.jsonObject(with: first) as? [String: Any])
        #expect(record["n"] as? Int == 3)
        #expect(result.lastSequence == 4)
        #expect(result.generation == "gen-1")
        #expect(result.complete)
        #expect(transport.routedEventCount == 0, "a journal line counted in the control sequence")
        #expect(events.lines.withLock { $0.isEmpty }, "a journal line reached onEvent")
        #expect(Self.routes(transport) == 0, "the stream_end left its route")
    }

    @Test func aRequestOnTheConnectionIsAnsweredWhileTheReadIsOpen() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())
        defer { transport.close() }

        let read = Self.read(on: transport)
        let open = try Self.readOpen(daemon)
        daemon.write([Self.ack(open.id, open.stream), Self.item(open.stream, 1)])

        let ping = Task { try await transport.request(cmd: "ping", timeout: .seconds(20)) { Data(#"{"id":\#($0),"cmd":"ping"}"#.utf8) } }
        let pingID = try daemon.readRequestID()
        daemon.write([Self.item(open.stream, 2), #"{"id":\#(pingID),"ok":true,"data":{}}"#])
        let reply = try await ping.value
        #expect(reply.eventBarrier == 0, "journal items moved a reply's event barrier")

        daemon.write([Self.end(open.stream, "completed")])
        let result = try await read.value
        #expect(result.records.count == 2 && result.complete)
    }

    @Test func aReadCutAtItsLimitCancelsItsStream() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())
        defer { transport.close() }

        let read = Self.read(on: transport, limit: 2)
        let open = try Self.readOpen(daemon)
        daemon.write([Self.ack(open.id, open.stream)] + (1...3).map { Self.item(open.stream, $0) })

        let cancel = try daemon.readRequest()
        #expect(cancel["operation"] as? String == "stream.cancel")
        #expect((cancel["params"] as? [String: Any])?["stream"] as? String == open.stream)
        let cancelID = try #require(cancel["id"] as? String)
        daemon.write([Self.end(open.stream, "canceled"),
                      #"{"protocol":"cmux.protocol/2","type":"response","id":"\#(cancelID)","ok":true,"result":{}}"#])
        let result = try await read.value

        #expect(result.records.count == 2)
        #expect(result.lastSequence == 2)
        #expect(!result.complete)
        #expect(Self.routes(transport) == 0)
    }

    @Test func aRefusedOpenLeavesNoRoute() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())
        defer { transport.close() }

        let read = Self.read(on: transport)
        let open = try Self.readOpen(daemon)
        daemon.write([#"{"protocol":"cmux.protocol/2","type":"response","id":"\#(open.id)","ok":false,"error":{"code":"operation.unsupported","message":"no journal"}}"#])

        await #expect(throws: DaemonError.self) { try await read.value }
        #expect(Self.routes(transport) == 0)
    }

    @Test func aClosedConnectionEndsTheReadWithWhatItHas() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())

        let read = Self.read(on: transport)
        let open = try Self.readOpen(daemon)
        daemon.write([Self.ack(open.id, open.stream), Self.item(open.stream, 1)])
        // The item is routed before the close: a ping sent after it is answered after it.
        let ping = Task { try await transport.request(cmd: "ping", timeout: .seconds(20)) { Data(#"{"id":\#($0),"cmd":"ping"}"#.utf8) } }
        let pingID = try daemon.readRequestID()
        daemon.write([#"{"id":\#(pingID),"ok":true,"data":{}}"#])
        _ = try await ping.value
        transport.close()
        let result = try await read.value

        #expect(result.records.count == 1)
        #expect(!result.complete)
        #expect(Self.routes(transport) == 0)
    }
}
