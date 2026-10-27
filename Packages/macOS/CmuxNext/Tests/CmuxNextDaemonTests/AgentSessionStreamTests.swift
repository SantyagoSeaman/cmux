import CmuxNextCompat
import Darwin
import Foundation
import Testing
@testable import CmuxNextDaemon

/// Agent panes as streams on the machine's one connection (p01-process-236): ten panes share it with no
/// connection of their own, each pane gets only its surface's events through its bounded queue, never as a
/// stream-0 event, and a pane falls back to a connection of its own only where the shared one cannot carry it.
@Suite(.serialized, .timeLimit(.minutes(1))) struct AgentSessionStreamTests {
    /// The daemon end of one connection: reads request objects and writes lines.
    final class Daemon {
        let path = "/tmp/cmux-asa-\(UUID().uuidString.prefix(8)).sock"
        private var listener: Int32 = -1
        private var peer: Int32 = -1

        init() throws {
            listener = socket(AF_UNIX, SOCK_STREAM, 0)
            var address = sockaddr_un()
            address.sun_family = sa_family_t(AF_UNIX)
            let path = path
            withUnsafeMutableBytes(of: &address.sun_path) { buffer in
                buffer.copyBytes(from: path.utf8)
                buffer[path.utf8.count] = 0
            }
            let bound = withUnsafePointer(to: &address) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    bind(listener, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
                }
            }
            guard bound == 0, listen(listener, 1) == 0 else { throw POSIXError(.EADDRINUSE) }
        }

        deinit {
            if peer >= 0 { close(peer) }
            if listener >= 0 { close(listener) }
            unlink(path)
        }

        func accept() throws {
            peer = Darwin.accept(listener, nil, nil)
            guard peer >= 0 else { throw POSIXError(.ECONNABORTED) }
        }

        /// The next request line as its JSON object.
        func readRequest() throws -> [String: Any] {
            var line = Data()
            var byte: UInt8 = 0
            while byte != 0x0A {
                guard read(peer, &byte, 1) == 1 else { throw POSIXError(.ECONNRESET) }
                line.append(byte)
            }
            return try #require(try JSONSerialization.jsonObject(with: line) as? [String: Any])
        }

        func write(_ lines: [String]) {
            var bytes = Array(lines.joined(separator: "\n").utf8)
            bytes.append(0x0A)
            var offset = 0
            while offset < bytes.count {
                let written = bytes[offset...].withUnsafeBytes { Darwin.write(peer, $0.baseAddress, $0.count) }
                guard written > 0 else { return }
                offset += written
            }
        }

        /// Answers the next request, which must be `cmd` on `surface`, with `data` (or a refusal `code`).
        func answer(_ cmd: String, surface: UInt64, data: String = Daemon.page, refusal code: String? = nil) throws {
            let request = try readRequest()
            #expect(request["cmd"] as? String == cmd)
            #expect((request["surface"] as? NSNumber)?.uint64Value == surface)
            let id = try #require((request["id"] as? NSNumber)?.uint64Value)
            if let code {
                write([#"{"id":\#(id),"ok":false,"error":"refused","error_code":"\#(code)"}"#])
            } else {
                write([#"{"id":\#(id),"ok":true,"data":\#(data)}"#])
            }
        }

        static let page = #"{"session":{"sessionId":"s"},"events":[],"hasMore":false,"lastSeq":0}"#
    }

    /// The pushes one pane's handler got, in order.
    final class Pushes: Sendable {
        let all = Mutex<[(event: String, line: Data)]>([])
        let arrivals: AsyncStream<Void>
        private let continuation: AsyncStream<Void>.Continuation

        init() {
            let (arrivals, continuation) = AsyncStream<Void>.makeStream()
            self.arrivals = arrivals
            self.continuation = continuation
        }

        var handler: AgentSessionAttachClient.EventHandler {
            { [self] event, line in
                all.withLock { $0.append((event, line)) }
                continuation.yield()
            }
        }

        func waitFor(_ count: Int) async {
            if all.withLock({ $0.count }) >= count { return }
            for await _ in arrivals where all.withLock({ $0.count }) >= count { return }
        }

        var events: [String] { all.withLock { $0.map(\.event) } }
        var reasons: [String?] {
            all.withLock { $0.map { ((try? JSONSerialization.jsonObject(with: $0.line)) as? [String: Any])?["reason"] as? String } }
        }
    }

    /// Counts the panes that asked for a connection of their own, and refuses each.
    final class OwnConnections: Sendable {
        struct Refused: Error {}
        let calls = Mutex(0)
        var endpoint: AgentSessionAttachClient.EndpointProvider {
            { [self] in
                calls.withLock { $0 += 1 }
                throw Refused()
            }
        }
    }

    /// A shared connection between two transports at the first ask (none ready in time), ready from the second.
    final class Reconnecting: Sendable {
        let asks = Mutex(0)
        let carrier: AgentSessionCarrier

        init(_ carrier: AgentSessionCarrier) { self.carrier = carrier }

        var provider: AgentSessionAttachClient.CarrierProvider {
            { [self] _ in
                let ask = asks.withLock { count -> Int in
                    count += 1
                    return count
                }
                guard ask > 1 else { throw DaemonError.notConnected }
                return carrier
            }
        }
    }

    /// Every event that reached stream 0 (the control connection's consumer).
    final class StreamZero: Sendable {
        let names = Mutex<[String]>([])
        let arrivals: AsyncStream<Void>
        private let continuation: AsyncStream<Void>.Continuation

        init() {
            let (arrivals, continuation) = AsyncStream<Void>.makeStream()
            self.arrivals = arrivals
            self.continuation = continuation
        }

        func record(_ name: String) {
            names.withLock { $0.append(name) }
            continuation.yield()
        }

        func waitFor(_ count: Int) async {
            if names.withLock({ $0.count }) >= count { return }
            for await _ in arrivals where names.withLock({ $0.count }) >= count { return }
        }
    }

    private static let identity = DaemonIdentity(capabilities: [DaemonCapability.agentSessionAttach.rawValue], generation: "agent-streams")

    private func connect(_ daemon: Daemon, streamZero: StreamZero = StreamZero()) throws -> AgentSessionCarrier {
        let transport = try LineTransport(path: daemon.path, role: .control, stats: nil)
        try daemon.accept()
        transport.start(onEvent: { name, _, _ in streamZero.record(name) }, onClose: { _ in })
        return AgentSessionCarrier(transport: transport, identity: Self.identity, ownsConnection: false)
    }

    private func client(_ surface: UInt64, on carrier: AgentSessionCarrier, own: OwnConnections,
                        limits: AgentSessionEventQueue.Limits = .init(), timeout: Duration = .seconds(5)) -> AgentSessionAttachClient {
        AgentSessionAttachClient(surface: surface, requestTimeout: timeout, mode: .attach, shared: { _ in carrier },
                                 queueLimits: limits, endpoint: own.endpoint)
    }

    /// A request the test sends on the shared connection after the pane's: the daemon reads it before any
    /// later line of the pane.
    private func probe(_ carrier: AgentSessionCarrier) -> Task<LineTransport.Response, any Error> {
        Task { try await carrier.transport.request(cmd: "ping", timeout: .seconds(5)) { Data(#"{"id":\#($0),"cmd":"ping"}"#.utf8) } }
    }

    private func answerProbe(_ daemon: Daemon, _ probe: Task<LineTransport.Response, any Error>) async throws {
        let request = try daemon.readRequest()
        #expect(request["cmd"] as? String == "ping", "the pane sent \(request) before its attach's answer")
        daemon.write([#"{"id":\#((request["id"] as? NSNumber)?.uint64Value ?? 0),"ok":true,"data":{}}"#])
        _ = try await probe.value
    }

    private static func record(_ surface: UInt64, seq: Int) -> String {
        #"{"event":"agent-session-record","surface":\#(surface),"record":{"seq":\#(seq)}}"#
    }

    /// Ten panes attach on the one connection: no pane asks for a connection of its own (so none opens a
    /// socket or a reader thread), the connection holds ten attachments, and each pane gets only its surface's
    /// records, none of them as a stream-0 event.
    @Test func tenPanesShareOneConnection() async throws {
        let daemon = try Daemon()
        let streamZero = StreamZero()
        let carrier = try connect(daemon, streamZero: streamZero)
        defer { carrier.transport.close() }
        let own = OwnConnections()
        var panes: [(client: AgentSessionAttachClient, pushes: Pushes)] = []
        for surface in UInt64(1)...10 {
            let pane = (client: client(surface, on: carrier, own: own), pushes: Pushes())
            let attached = Task { [pane] in
                try await pane.client.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: pane.pushes.handler)
            }
            try daemon.answer("agent-session-attach", surface: surface)
            _ = try await attached.value
            panes.append(pane)
        }
        #expect(own.calls.withLock { $0 } == 0, "a pane opened a connection of its own")
        #expect(carrier.transport.agentSessionCount == 10)

        daemon.write([Self.record(3, seq: 1), Self.record(7, seq: 1), Self.record(3, seq: 2),
                      #"{"event":"workspace-changed"}"#])
        await panes[2].pushes.waitFor(2)
        await panes[6].pushes.waitFor(1)
        await streamZero.waitFor(1)
        #expect(panes[2].pushes.events == ["agent-session-record", "agent-session-record"])
        #expect(panes[6].pushes.events == ["agent-session-record"])
        #expect(panes.enumerated().allSatisfy { index, pane in [2, 6].contains(index) || pane.pushes.events.isEmpty })
        #expect(streamZero.names.withLock { $0 } == ["workspace-changed"], "an agent record reached stream 0")
        #expect(carrier.transport.routedEventCount == 1)
        for pane in panes { await pane.client.close() }
    }

    /// The daemon's per-connection limit on the shared connection sends the pane to a connection of its own,
    /// and its refused stream lets go of the surface there.
    @Test func aLimitOnTheSharedConnectionFallsBackToAConnectionOfItsOwn() async throws {
        let daemon = try Daemon()
        let carrier = try connect(daemon)
        defer { carrier.transport.close() }
        let own = OwnConnections()
        let pane = client(4, on: carrier, own: own)
        let attached = Task { try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil) { _, _ in } }
        try daemon.answer("agent-session-attach", surface: 4, refusal: "agent_session.limit")
        await #expect(throws: OwnConnections.Refused.self) { try await attached.value }
        #expect(own.calls.withLock { $0 } == 1)
        #expect(carrier.transport.agentSessionCount == 0)
    }

    /// A second pane of the same tab cannot share the surface's one attachment on the connection: it takes a
    /// connection of its own, and its detach without an attachment never ends the first pane's.
    @Test func aSecondPaneOfOneTabNeverTakesTheFirstPanesAttachment() async throws {
        let daemon = try Daemon()
        let carrier = try connect(daemon)
        defer { carrier.transport.close() }
        let own = OwnConnections()
        let first = client(5, on: carrier, own: own)
        let attached = Task { try await first.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil) { _, _ in } }
        try daemon.answer("agent-session-attach", surface: 5)
        _ = try await attached.value

        let second = client(5, on: carrier, own: own)
        await #expect(throws: OwnConnections.Refused.self) {
            _ = try await second.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil) { _, _ in }
        }
        #expect(own.calls.withLock { $0 } == 1)
        await second.detach()
        await second.close()
        #expect(carrier.transport.agentSessionCount == 1, "the second pane ended the first pane's attachment")

        // The first pane's detach is the next line the daemon reads: nothing came from the second pane.
        let detached = Task { await first.detach() }
        try daemon.answer("agent-session-detach", surface: 5, data: "{}")
        await detached.value
        #expect(carrier.transport.agentSessionCount == 0)
    }

    /// A pane that falls behind its bounded queue ends with `overflow`: the daemon gets a detach, so it stops
    /// sending, and the pane gets the close it reattaches on.
    @Test func aPaneThatFallsBehindEndsWithOverflow() async throws {
        let daemon = try Daemon()
        let carrier = try connect(daemon)
        defer { carrier.transport.close() }
        let pushes = Pushes()
        let pane = client(9, on: carrier, own: OwnConnections(), limits: .init(events: 16, bytes: 32))
        let attached = Task { try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: pushes.handler) }
        try daemon.answer("agent-session-attach", surface: 9)
        _ = try await attached.value

        daemon.write([Self.record(9, seq: 1)])  // longer than the 32-byte bound
        let detach = try daemon.readRequest()
        #expect(detach["cmd"] as? String == "agent-session-detach")
        #expect((detach["surface"] as? NSNumber)?.uint64Value == 9)
        await pushes.waitFor(1)
        #expect(pushes.events == ["agent-session-closed"])
        #expect(pushes.reasons == ["overflow"])
        #expect(carrier.transport.agentSessionCount == 0)
        await pane.close()
    }

    /// The daemon's `agent-session-closed` is the attachment's last event: the connection forgets the surface,
    /// and a close of the client afterwards sends no detach (it would end a later attachment of the surface).
    @Test func theDaemonsCloseEndsTheStream() async throws {
        let daemon = try Daemon()
        let carrier = try connect(daemon)
        defer { carrier.transport.close() }
        let pushes = Pushes()
        let pane = client(2, on: carrier, own: OwnConnections())
        let attached = Task { try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: pushes.handler) }
        try daemon.answer("agent-session-attach", surface: 2)
        _ = try await attached.value

        daemon.write([Self.record(2, seq: 1), #"{"event":"agent-session-closed","surface":2,"reason":"detached"}"#])
        await pushes.waitFor(2)
        #expect(pushes.events == ["agent-session-record", "agent-session-closed"])
        #expect(carrier.transport.agentSessionCount == 0)
        await pane.close()

        // The next line the daemon reads is this probe's, not a detach.
        let probe = Task { try await carrier.transport.request(cmd: "ping", timeout: .seconds(5)) { Data(#"{"id":\#($0),"cmd":"ping"}"#.utf8) } }
        let request = try daemon.readRequest()
        #expect(request["cmd"] as? String == "ping")
        daemon.write([#"{"id":\#((request["id"] as? NSNumber)?.uint64Value ?? 0),"ok":true,"data":{}}"#])
        _ = try await probe.value
    }

    /// A pane closed while its attach waits for the daemon's answer sends no detach then (a daemon that is still
    /// attaching would answer it and attach after it), and sends one after the answer: no attachment of a closed
    /// pane is left on the shared connection.
    @Test func aPaneClosedBeforeItsAttachAnswersDetachesAfterTheAnswer() async throws {
        let daemon = try Daemon()
        let carrier = try connect(daemon)
        defer { carrier.transport.close() }
        let pushes = Pushes()
        let pane = client(6, on: carrier, own: OwnConnections())
        let attached = Task { try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: pushes.handler) }
        let request = try daemon.readRequest()
        #expect(request["cmd"] as? String == "agent-session-attach")
        await pane.close()

        // The next line the daemon reads is the probe's: the close sent nothing yet.
        try await answerProbe(daemon, probe(carrier))
        daemon.write([#"{"id":\#((request["id"] as? NSNumber)?.uint64Value ?? 0),"ok":true,"data":\#(Daemon.page)}"#])
        let detach = try daemon.readRequest()
        #expect(detach["cmd"] as? String == "agent-session-detach")
        #expect((detach["surface"] as? NSNumber)?.uint64Value == 6)
        await #expect(throws: AgentSessionAttachError.self) { try await attached.value }
        #expect(carrier.transport.agentSessionCount == 0)
        #expect(pushes.events.isEmpty)
    }

    /// An attach with no answer in time may still attach on the daemon: the pane detaches it, and lets go of its
    /// surface on the connection.
    @Test func anAttachWithNoAnswerInTimeDetaches() async throws {
        let daemon = try Daemon()
        let carrier = try connect(daemon)
        defer { carrier.transport.close() }
        let pane = client(8, on: carrier, own: OwnConnections(), timeout: .milliseconds(200))
        let attached = Task { try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil) { _, _ in } }
        let request = try daemon.readRequest()
        #expect(request["cmd"] as? String == "agent-session-attach")
        await #expect(throws: DaemonError.self) { try await attached.value }

        // A probe after the timeout: the pane's detach is read before it.
        let after = probe(carrier)
        let detach = try daemon.readRequest()
        #expect(detach["cmd"] as? String == "agent-session-detach", "the timed-out attach was forgotten with no detach")
        #expect((detach["surface"] as? NSNumber)?.uint64Value == 8)
        #expect(carrier.transport.agentSessionCount == 0)
        if detach["cmd"] as? String == "ping" {
            daemon.write([#"{"id":\#((detach["id"] as? NSNumber)?.uint64Value ?? 0),"ok":true,"data":{}}"#])
        } else {
            try await answerProbe(daemon, after)
        }
        await pane.close()
    }

    /// While the shared connection reconnects the pane waits for the next one: a provider with no connection in
    /// time fails the attach (the page attaches again), and no pane opens a connection of its own for it.
    @Test func aReconnectingSharedConnectionNeverOpensOneOfThePanesOwn() async throws {
        let daemon = try Daemon()
        let carrier = try connect(daemon)
        defer { carrier.transport.close() }
        let own = OwnConnections()
        let reconnecting = Reconnecting(carrier)
        let pane = AgentSessionAttachClient(surface: 3, requestTimeout: .seconds(5), mode: .attach, shared: reconnecting.provider,
                                            queueLimits: .init(), endpoint: own.endpoint)
        await #expect(throws: DaemonError.notConnected) {
            _ = try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil) { _, _ in }
        }
        let attached = Task { try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil) { _, _ in } }
        try daemon.answer("agent-session-attach", surface: 3)
        _ = try await attached.value
        #expect(own.calls.withLock { $0 } == 0, "a pane opened a connection of its own for a reconnect")
        #expect(carrier.transport.agentSessionCount == 1)
        await pane.close()
    }

    /// An agent record is routed by the `surface` the line's byte scan reads: however long the record, routing
    /// it parses no JSON on the shared reader.
    @Test func aLongRecordIsRoutedWithNoDecode() async throws {
        let daemon = try Daemon()
        let stats = DaemonWireStats()
        let transport = try LineTransport(path: daemon.path, role: .control, stats: stats)
        try daemon.accept()
        transport.start(onEvent: { _, _, _ in }, onClose: { _ in })
        defer { transport.close() }
        let carrier = AgentSessionCarrier(transport: transport, identity: Self.identity, ownsConnection: false)
        let pushes = Pushes()
        let pane = client(11, on: carrier, own: OwnConnections(), limits: .init(events: 16, bytes: 8 << 20))
        let attached = Task { try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: pushes.handler) }
        try daemon.answer("agent-session-attach", surface: 11)
        _ = try await attached.value
        let before = stats.snapshot().jsonDecodes

        let text = String(repeating: "a", count: 4 << 20)
        daemon.write([#"{"event":"agent-session-record","record":{"seq":1,"text":"\#(text)","surface":99},"surface":11}"#])
        await pushes.waitFor(1)
        #expect(pushes.events == ["agent-session-record"])
        #expect(stats.snapshot().jsonDecodes == before, "routing the record decoded it on the reader")
        await pane.close()
    }

    /// The shared connection's end ends every pane on it with `connection_lost`.
    @Test func theConnectionsEndEndsEveryPane() async throws {
        let daemon = try Daemon()
        let carrier = try connect(daemon)
        var pushes: [Pushes] = []
        var clients: [AgentSessionAttachClient] = []
        for surface in UInt64(1)...3 {
            let paneHandler = Pushes()
            let pane = client(surface, on: carrier, own: OwnConnections())
            let attached = Task { try await pane.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: paneHandler.handler) }
            try daemon.answer("agent-session-attach", surface: surface)
            _ = try await attached.value
            pushes.append(paneHandler)
            clients.append(pane)
        }
        carrier.transport.close()
        for paneHandler in pushes {
            await paneHandler.waitFor(1)
            #expect(paneHandler.events == ["agent-session-closed"])
            #expect(paneHandler.reasons == ["connection_lost"])
        }
        for pane in clients { await pane.close() }
    }
}
