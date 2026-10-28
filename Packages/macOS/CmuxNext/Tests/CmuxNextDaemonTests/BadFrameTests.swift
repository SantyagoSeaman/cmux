import CmuxNextCompat
import CmuxNextWakeupsTestSupport
@testable import CmuxNextDaemon
import Darwin
import Foundation
import Testing

/// Bad messages from the daemon and the crash-loop breaker (p05-ipc-191). Each of 20 malformed messages closes
/// the connection with its reason and one `badMessages`, and nothing crashes. Three bad-message closes of
/// framed connections to one daemon life within 60 s turn frames off for that life: its next handshake asks
/// for none, the trip is counted, and the diagnostic reaches the installed sink.
@Suite(.serialized, .timeLimit(.minutes(1))) struct BadFrameTests {
    /// One accepted connection on a Unix socket in `/tmp` that writes raw bytes.
    final class RawDaemon {
        let path = "/tmp/cnd-bad-\(UUID().uuidString.prefix(8).lowercased()).sock"
        private var listener: Int32 = -1
        private var peer: Int32 = -1

        init() throws {
            listener = socket(AF_UNIX, SOCK_STREAM, 0)
            var address = sockaddr_un()
            address.sun_family = sa_family_t(AF_UNIX)
            let pathBytes = Array(path.utf8)
            withUnsafeMutableBytes(of: &address.sun_path) { buffer in
                buffer.copyBytes(from: pathBytes)
                buffer[pathBytes.count] = 0
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

        /// Accepts the transport's connection (already queued by its connect).
        func accept() throws {
            peer = Darwin.accept(listener, nil, nil)
            guard peer >= 0 else { throw POSIXError(.ECONNABORTED) }
            var on: Int32 = 1
            setsockopt(peer, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        }

        /// Writes `bytes`; stops early once the app closed its end.
        func send(_ bytes: [UInt8]) {
            var offset = 0
            while offset < bytes.count {
                let written = bytes[offset...].withUnsafeBytes { Darwin.write(peer, $0.baseAddress, $0.count) }
                guard written > 0 else { return }
                offset += written
            }
        }

        /// Closes the daemon's side for writing: the app reads the end of the stream.
        func endWrites() {
            shutdown(peer, SHUT_WR)
        }
    }

    /// The diagnostics the breaker's sink got.
    final class Shown: Sendable {
        let trips = Mutex<[DaemonFrameBreaker.Trip]>([])
    }

    /// A malformed message and the reason its close names.
    struct Fixture: Sendable, CustomTestStringConvertible {
        let name: String
        let bytes: [UInt8]
        /// The connection asked for frames before the bytes came.
        let framed: Bool
        let reason: String

        var testDescription: String { name }
    }

    /// A frame's 16 header bytes and its payload, valid or not.
    /// `aux` defaults to a valid call aux (``call(priority:flags:)``).
    static func frame(kind: UInt8, stream: UInt16 = 0, length: UInt32 = 0, aux: [UInt8]? = nil,
                      payload: [UInt8] = []) -> [UInt8] {
        var bytes: [UInt8] = [0xFF, kind, UInt8(truncatingIfNeeded: stream), UInt8(truncatingIfNeeded: stream >> 8)]
        for byte in 0..<4 { bytes.append(UInt8(truncatingIfNeeded: length >> (8 * UInt32(byte)))) }
        bytes += aux ?? call()
        bytes += payload
        return bytes
    }

    /// A call kind's aux: request id 1, method 0x0100, `priority` and `flags`.
    static func call(priority: UInt8 = 1, flags: UInt8 = 0) -> [UInt8] {
        [1, 0, 0, 0, 0x00, 0x01, priority, flags]
    }

    static let fixtures: [Fixture] = {
        let zero = [UInt8](repeating: 0, count: 8)
        func framed(_ name: String, _ bytes: [UInt8], _ reason: String) -> Fixture {
            Fixture(name: name, bytes: bytes, framed: true, reason: reason)
        }
        func lines(_ name: String, _ bytes: [UInt8], _ reason: String) -> Fixture {
            Fixture(name: name, bytes: bytes, framed: false, reason: reason)
        }
        return [
            framed("kind 0", BadFrameTests.frame(kind: 0, aux: zero), "unknown_kind"),
            framed("kind 12", BadFrameTests.frame(kind: 12, stream: 1, aux: zero), "unknown_kind"),
            framed("kind 255", BadFrameTests.frame(kind: 255, aux: zero), "unknown_kind"),
            framed("reply over 32 MiB", BadFrameTests.frame(kind: 17, length: (32 << 20) + 1), "oversize"),
            framed("priority 4", BadFrameTests.frame(kind: 17, aux: BadFrameTests.call(priority: 4)), "unknown_priority"),
            framed("flag 0x20", BadFrameTests.frame(kind: 17, aux: BadFrameTests.call(flags: 0x20)), "unknown_flags"),
            framed("output on stream 0", BadFrameTests.frame(kind: 1, length: 1, aux: zero, payload: [0x78]), "data_on_control_stream"),
            framed("input on stream 0", BadFrameTests.frame(kind: 9, length: 1, aux: zero, payload: [0x78]), "data_on_control_stream"),
            framed("ring ack on stream 0", BadFrameTests.frame(kind: 11, aux: zero), "data_on_control_stream"),
            framed("priority 255", BadFrameTests.frame(kind: 18, aux: BadFrameTests.call(priority: 255)), "unknown_priority"),
            framed("flag 0x80", BadFrameTests.frame(kind: 18, aux: BadFrameTests.call(flags: 0x80)), "unknown_flags"),
            framed("request", BadFrameTests.frame(kind: 16, length: 2, payload: Array("{}".utf8)), "unrouted frame kind 16 on stream 0"),
            framed("typed event", BadFrameTests.frame(kind: 19, length: 2, payload: Array("{}".utf8)), "unrouted frame kind 19 on stream 0"),
            framed("cancel", BadFrameTests.frame(kind: 20), "unrouted frame kind 20 on stream 0"),
            framed("output on a stream never opened", BadFrameTests.frame(kind: 1, stream: 3, length: 1, aux: zero, payload: [0x78]),
                   "unrouted frame kind 1 on stream 3"),
            framed("reply on a stream", BadFrameTests.frame(kind: 17, stream: 5, length: 2, payload: Array("{}".utf8)),
                   "unrouted frame kind 17 on stream 5"),
            lines("frame before negotiation", BadFrameTests.frame(kind: 17, length: 2, payload: Array("{}".utf8)), "frames_not_negotiated"),
            lines("line with a bad sequence", [0x7B, 0xC3, 0x28, 0x7D, 0x0A], "invalid_utf8"),
            lines("line with a lone continuation byte", [0x80, 0x0A], "invalid_utf8"),
            lines("line with an overlong encoding", [0x7B, 0xC0, 0xAF, 0x7D, 0x0A], "invalid_utf8"),
        ]
    }()

    /// A line that routes, sent before each bad message.
    static let goodLine = Array("{\"event\":\"first\"}\n".utf8)

    /// Opens a transport to `daemon` counting into `stats`; with `framed` it asks for frames first, reporting a
    /// bad message to `watch`. Returns the transport, its close and its routed event names.
    static func open(_ daemon: RawDaemon, stats: DaemonWireStats, framed: Bool, watch: DaemonFrameBreaker.Watch? = nil)
        throws -> (LineTransport, AsyncStream<TransportCloseReason>, AsyncStream<String>) {
        let transport = try LineTransport(path: daemon.path, role: .control, stats: stats)
        try daemon.accept()
        if framed { transport.acceptFrames(reportingTo: watch) }
        let (closed, closing) = AsyncStream.makeStream(of: TransportCloseReason.self)
        let (events, routing) = AsyncStream.makeStream(of: String.self)
        transport.start(onEvent: { name, _, _ in routing.yield(name) },
                        onClose: { reason in
                            closing.yield(reason)
                            closing.finish()
                            routing.finish()
                        })
        return (transport, closed, events)
    }

    static func first<Element>(_ stream: AsyncStream<Element>) async -> Element? {
        var iterator = stream.makeAsyncIterator()
        return await iterator.next()
    }

    @Test func thereAreTwentyFixtures() {
        #expect(Self.fixtures.count == 20)
    }

    /// The good line before it routes; the bad message closes the connection with its reason, counts once, and
    /// nothing after it is read.
    @Test(arguments: BadFrameTests.fixtures) func aBadMessageClosesWithItsReason(_ fixture: Fixture) async throws {
        let daemon = try RawDaemon()
        let stats = DaemonWireStats()
        let (transport, closed, events) = try Self.open(daemon, stats: stats, framed: fixture.framed)
        defer { transport.close() }
        daemon.send(Self.goodLine)
        // A line after the bad message must never route.
        daemon.send(fixture.bytes + Self.goodLine)
        daemon.endWrites()
        let reason = await Self.first(closed)
        #expect(reason == .lost("bad message: \(fixture.reason)"))
        var routed: [String] = []
        for await name in events { routed.append(name) }
        #expect(routed == ["first"])
        let snapshot = stats.snapshot()
        #expect(snapshot.badMessages == 1)
        #expect(snapshot.frameBreakerTrips == 0)
    }

    /// The breaker counts each daemon life apart, trips on the third close inside 60 s, and trips once.
    @Test func theBreakerTripsOnTheThirdCloseOfALifeWithinTheWindow() {
        let clock = ManualClock()
        let breaker = DaemonFrameBreaker(clock: clock)
        #expect(breaker.record("unknown_kind", life: "1-a") == .counted)
        clock.advance(by: .seconds(30))
        #expect(breaker.record("unknown_flags", life: "1-a") == .counted)
        #expect(breaker.record("unknown_kind", life: "2-b") == .counted)
        clock.advance(by: .seconds(30))
        #expect(breaker.record("oversize", life: "1-a") == .tripped)
        #expect(breaker.record("oversize", life: "1-a") == .alreadyOff)
        #expect(breaker.record("oversize", life: "2-b") == .counted)

        // Closes 31 s apart never have three inside the window.
        let spread = DaemonFrameBreaker(clock: clock)
        for _ in 0..<6 {
            #expect(spread.record("unknown_kind", life: "3-c") == .counted)
            clock.advance(by: .seconds(31))
        }
    }

    /// Three framed connections to one daemon life close on bad frames: the third trips the breaker, which counts
    /// the trip and hands the diagnostic to the sink, and the next handshake with that life asks for no frames
    /// (streams still) while a new daemon life gets frames again.
    @Test func threeFramedBadMessagesTurnTheLifesFramesOffAndShowTheDiagnostic() async throws {
        let breaker = DaemonFrameBreaker()
        let shown = Shown()
        breaker.trips.install { trip in shown.trips.withLock { $0.append(trip) } }
        let upgrades = DaemonWireUpgrades(frames: DaemonHandshakeTests.schema, streams: true, frameBreaker: breaker)
        let negotiation = WireNegotiation(upgrades: upgrades, bridged: false)
        let offers = [WireNegotiation.framesCapability, WireNegotiation.streamsCapability]
        let identity = DaemonIdentity(capabilities: offers, pid: 4242, generation: DaemonGeneration(rawValue: "boot-a"))
        let stats = DaemonWireStats()
        for close in 1...DaemonFrameBreaker.tripCloses {
            let echo = try #require(negotiation.echo(offered: identity))
            #expect(echo.asksFrames, "close \(close): frames are still asked")
            #expect(shown.trips.withLock { $0.isEmpty }, "close \(close): no diagnostic yet")
            let daemon = try RawDaemon()
            let (transport, closed, _) = try Self.open(daemon, stats: stats, framed: true, watch: echo.breakerWatch)
            daemon.send(Self.frame(kind: 0, aux: [UInt8](repeating: 0, count: 8)))
            let reason = await Self.first(closed)
            // The close that trips the breaker says so in the connection's state.
            let detail = close == DaemonFrameBreaker.tripCloses
                ? "bad message: unknown_kind; \(DaemonFrameBreaker.tripDetail)" : "bad message: unknown_kind"
            #expect(reason == .lost(detail))
            transport.close()
        }
        let snapshot = stats.snapshot()
        #expect(snapshot.badMessages == 3)
        #expect(snapshot.frameBreakerTrips == 1)
        let expected = DaemonFrameBreaker.Trip(daemonLife: "4242-boot-a", reasons: ["unknown_kind", "unknown_kind", "unknown_kind"])
        #expect(shown.trips.withLock { $0 } == [expected])

        let fallback = try #require(negotiation.echo(offered: identity))
        #expect(!fallback.asksFrames)
        #expect(fallback.streams)
        #expect(fallback.breakerWatch == nil)
        let restarted = DaemonIdentity(capabilities: offers, pid: 4243, generation: DaemonGeneration(rawValue: "boot-b"))
        #expect(negotiation.echo(offered: restarted)?.asksFrames == true)
    }

    /// The daemon drops a slow reader at its write timeout, or dies, in the middle of a frame: a lost connection,
    /// not a bad message. Three such closes of one daemon life within the window count nothing and leave its
    /// frames on.
    @Test func theDaemonClosingInsideAFrameIsALostConnectionThatNeverTripsTheBreaker() async throws {
        let breaker = DaemonFrameBreaker()
        let identity = DaemonIdentity(capabilities: [WireNegotiation.framesCapability], pid: 9,
                                      generation: DaemonGeneration(rawValue: "boot-d"))
        let stats = DaemonWireStats()
        let reply = Self.frame(kind: 17, length: 10, payload: Array("{\"ok\":true".utf8))
        for cut in [9, 20, 9] {
            let daemon = try RawDaemon()
            let (transport, closed, events) = try Self.open(daemon, stats: stats, framed: true, watch: breaker.watch(identity))
            daemon.send(Self.goodLine + Array(reply.prefix(cut)))
            daemon.endWrites()
            let reason = await Self.first(closed)
            #expect(reason == .lost("EOF inside frame"), "cut at \(cut)")
            var routed: [String] = []
            for await name in events { routed.append(name) }
            #expect(routed == ["first"])
            transport.close()
        }
        let snapshot = stats.snapshot()
        #expect(snapshot.badMessages == 0)
        #expect(snapshot.frameBreakerTrips == 0)
        #expect(!breaker.framesOff(for: identity))
    }

    /// Bytes cut short by the app's own close are not the daemon's bad message: nothing counts.
    @Test func theAppsOwnCloseInsideAFrameCountsNothing() async throws {
        let breaker = DaemonFrameBreaker()
        let identity = DaemonIdentity(capabilities: [WireNegotiation.framesCapability], pid: 7,
                                      generation: DaemonGeneration(rawValue: "boot-c"))
        let stats = DaemonWireStats()
        for _ in 1...DaemonFrameBreaker.tripCloses {
            let daemon = try RawDaemon()
            let (transport, closed, events) = try Self.open(daemon, stats: stats, framed: true, watch: breaker.watch(identity))
            daemon.send(Self.goodLine + Array(Self.frame(kind: 17, length: 10).prefix(9)))
            _ = await Self.first(events)
            transport.close()
            let reason = await Self.first(closed)
            #expect(reason == .closedByClient)
        }
        #expect(stats.snapshot().badMessages == 0)
        #expect(!breaker.framesOff(for: identity))
    }
}
