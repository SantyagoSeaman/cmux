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

    private static func read(on transport: LineTransport, limit: Int = 20_000,
                             deadline: Duration = .seconds(20)) -> Task<SessionJournalRead.Result, any Error> {
        Task { try await SessionJournalRead(carrier: transport).read(kinds: ["agent.session"], cursor: nil, limit: limit, deadline: deadline) }
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

    private static func ok(_ id: String) -> String {
        #"{"protocol":"cmux.protocol/2","type":"response","id":"\#(id)","ok":true,"result":{}}"#
    }

    /// Reads the next request, checks it is `stream.cancel` of `stream`, and returns its id.
    private static func readCancel(_ daemon: FakeDaemon, of stream: String) throws -> String {
        let cancel = try daemon.readRequest()
        #expect(cancel["operation"] as? String == "stream.cancel")
        let params = try #require(cancel["params"] as? [String: Any])
        #expect(params["stream"] as? String == stream)
        // The daemon resolves both selectors before it cancels (resource_connection.rs, cancel_resource_stream).
        #expect(params["machine"] as? String == "current" && params["session"] as? String == "current")
        return try #require(cancel["id"] as? String)
    }

    private static func isTimedOut(_ error: DaemonError?) -> Bool {
        if case .timedOut? = error { true } else { false }
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

        let cancelID = try Self.readCancel(daemon, of: open.stream)
        daemon.write([Self.end(open.stream, "canceled"), Self.ok(cancelID)])
        let result = try await read.value

        #expect(result.records.count == 2)
        #expect(result.lastSequence == 2)
        #expect(!result.complete)
        #expect(Self.routes(transport) == 0)
        #expect(!transport.isClosed, "a confirmed cancel closed the connection")
    }

    @Test func aCancelThatEndsTheStreamOtherwiseClosesTheConnection() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())
        defer { transport.close() }

        let read = Self.read(on: transport, limit: 1)
        let open = try Self.readOpen(daemon)
        daemon.write([Self.ack(open.id, open.stream), Self.item(open.stream, 1), Self.item(open.stream, 2)])
        let cancelID = try Self.readCancel(daemon, of: open.stream)
        daemon.write([Self.end(open.stream, "completed"), Self.ok(cancelID)])
        _ = try await read.value

        #expect(transport.isClosed, "a cancel answered by stream_end(completed) left the connection in use")
    }

    @Test func anOpenWithNoAckCancelsItsStream() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())
        defer { transport.close() }

        let read = Self.read(on: transport, deadline: .seconds(1))
        let open = try Self.readOpen(daemon)
        let cancelID = try Self.readCancel(daemon, of: open.stream)
        daemon.write([Self.ok(cancelID), Self.end(open.stream, "canceled")])

        let error = await #expect(throws: DaemonError.self) { try await read.value }
        #expect(Self.isTimedOut(error), "the caller did not see the open's own error")
        #expect(Self.routes(transport) == 0)
        #expect(!transport.isClosed, "a confirmed cancel closed the connection")
    }

    @Test func anAckForAnotherStreamIsNotAnOpen() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())
        defer { transport.close() }

        let read = Self.read(on: transport)
        let open = try Self.readOpen(daemon)
        daemon.write([Self.ack(open.id, "stream_someoneelse")])
        let cancelID = try Self.readCancel(daemon, of: open.stream)
        daemon.write([Self.end(open.stream, "canceled"), Self.ok(cancelID)])

        let error = await #expect(throws: DaemonError.self) { try await read.value }
        if case .malformedResponse? = error {} else { Issue.record("expected malformedResponse, got \(String(describing: error))") }
        #expect(Self.routes(transport) == 0)
        #expect(!transport.isClosed)
    }

    @Test func aStreamThatEndedOnItsOwnSendsNoCancel() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())
        defer { transport.close() }

        transport.routeStream("stream_done") { _ in }
        // The end is routed before the ping's reply, so the stream is over when the ping returns.
        let ping = Task { try await transport.request(cmd: "ping", timeout: .seconds(20)) { Data(#"{"id":\#($0),"cmd":"ping"}"#.utf8) } }
        let pingID = try daemon.readRequestID()
        daemon.write([Self.end("stream_done", "completed"), #"{"id":\#(pingID),"ok":true,"data":{}}"#])
        _ = try await ping.value

        await transport.cancelResourceStream("stream_done", timeout: .seconds(1))
        let next = Task { try await transport.request(cmd: "ping", timeout: .seconds(20)) { Data(#"{"id":\#($0),"cmd":"ping"}"#.utf8) } }
        let request = try daemon.readRequest()
        #expect(request["cmd"] as? String == "ping", "an ended stream sent \(request)")
        let nextID = try #require((request["id"] as? NSNumber)?.uint64Value)
        daemon.write([#"{"id":\#(nextID),"ok":true,"data":{}}"#])
        _ = try await next.value
        #expect(!transport.isClosed)
    }

    @Test func anUnconfirmedCancelClosesTheConnection() async throws {
        let daemon = try FakeDaemon()
        let transport = try Self.connect(daemon, events: ReplyEventOrderTests.Routed())
        defer { transport.close() }

        let read = Self.read(on: transport, deadline: .seconds(1))
        let open = try Self.readOpen(daemon)
        // The cancel is answered, but its stream_end never comes.
        let cancelID = try Self.readCancel(daemon, of: open.stream)
        daemon.write([Self.ok(cancelID)])

        let error = await #expect(throws: DaemonError.self) { try await read.value }
        #expect(Self.isTimedOut(error))
        #expect(transport.isClosed, "a cancel with no stream_end left the connection in use")
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
        // An ok:false open is conclusive: no stream.cancel follows it, so the next request is this ping.
        let ping = Task { try await transport.request(cmd: "ping", timeout: .seconds(20)) { Data(#"{"id":\#($0),"cmd":"ping"}"#.utf8) } }
        let next = try daemon.readRequest()
        #expect(next["cmd"] as? String == "ping", "a refused open sent \(next)")
        let pingID = try #require((next["id"] as? NSNumber)?.uint64Value)
        daemon.write([#"{"id":\#(pingID),"ok":true,"data":{}}"#])
        _ = try await ping.value
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
