import CmuxNextCompat
import Foundation
import Testing
@testable import CmuxNextDaemon

/// The app end of `ReportBadMessage` (p10-stability-178): a bad message from the daemon closes only the
/// connection it came on, counts its reason, and reports each reason once per ledger.
@Suite struct BadMessageReportsTests {
    /// The reports a ledger sent to its sink.
    final class Sunk: Sendable {
        let reports = Mutex<[BadMessageReports.Report]>([])

        var all: [BadMessageReports.Report] { reports.withLock { $0 } }

        func ledger() -> BadMessageReports {
            BadMessageReports { report in self.reports.withLock { $0.append(report) } }
        }
    }

    /// A bridge child that dials, then writes `output` and waits for one stdin line per later write.
    private static func bridge(_ output: String, after: [String] = []) -> DaemonBridge {
        var lines = ["import sys", "print('{\"ok\":true}', file=sys.stderr, flush=True)",
                     "sys.stdout.buffer.write(\(output)); sys.stdout.flush()"]
        for write in after {
            lines.append("sys.stdin.readline()")
            lines.append("sys.stdout.buffer.write(\(write)); sys.stdout.flush()")
        }
        lines.append("sys.stdin.readline()")
        return DaemonBridge(executable: "/usr/bin/python3", arguments: ["-u", "-c", lines.joined(separator: "\n")])
    }

    private static func socketPath(_ label: String) -> String {
        "/tmp/cmux-bad-message-\(label)-\(UUID().uuidString).sock"
    }

    @Test func eachReasonIsReportedOnceAndCountedEveryTime() {
        let sunk = Sunk()
        let ledger = sunk.ledger()
        #expect(ledger.report(peer: "daemon control", reason: "bad_magic", detail: "badMagic(found: 65)") == .first)
        #expect(ledger.report(peer: "daemon terminal_attach", reason: "bad_magic", detail: "badMagic(found: 66)") == .repeated)
        #expect(ledger.report(peer: "daemon control", reason: "truncated", detail: "truncated") == .first)
        #expect(ledger.count("bad_magic") == 2)
        #expect(ledger.count("truncated") == 1)
        #expect(ledger.count("oversize") == 0)
        #expect(ledger.all == ["bad_magic": 2, "truncated": 1])
        #expect(sunk.all == [
            .init(peer: "daemon control", reason: "bad_magic", detail: "badMagic(found: 65)"),
            .init(peer: "daemon control", reason: "truncated", detail: "truncated"),
        ])
    }

    @Test func ledgersAreIndependent() {
        let first = Sunk(), second = Sunk()
        _ = first.ledger().report(peer: "daemon control", reason: "oversize", detail: "")
        #expect(second.ledger().report(peer: "daemon control", reason: "oversize", detail: "") == .first)
        #expect(first.all.count == 1)
        #expect(second.all.count == 1)
    }

    /// The ledger rides on the endpoint, outside its identity.
    @Test func anEndpointCarriesItsLedgerOutsideItsIdentity() {
        let ledger = BadMessageReports()
        let carried = DaemonEndpoint(socketPath: "/tmp/cmux.sock", badMessages: ledger)
        #expect(carried.badMessages === ledger)
        #expect(carried == DaemonEndpoint(socketPath: "/tmp/cmux.sock"))
        #expect(carried.hashValue == DaemonEndpoint(socketPath: "/tmp/cmux.sock").hashValue)
    }

    /// A frame before frames were negotiated is a bad message: that connection closes and is reported, and a
    /// second connection to the daemon keeps routing.
    @Test func aMalformedFrameClosesOnlyItsConnection() async throws {
        let sunk = Sunk()
        let ledger = sunk.ledger()
        let good = try await LineTransport.open(
            path: Self.socketPath("good"), bridge: Self.bridge(#"b'{"event":"before"}\n'"#, after: [#"b'{"event":"after"}\n'"#]),
            role: .control, stats: nil, badMessages: ledger)
        defer { good.close() }
        let bad = try await LineTransport.open(
            path: Self.socketPath("bad"), bridge: Self.bridge(#"b'{"event":"first"}\n\xff\x00{"event":"never"}\n'"#),
            role: .terminalAttach, stats: nil, badMessages: ledger)
        defer { bad.close() }

        let routed = ReplyEventOrderTests.Routed()
        good.start(onEvent: { name, _, index in routed.record(name, index) }, onClose: { _ in })
        var arrivals = routed.arrivals.makeAsyncIterator()
        _ = await arrivals.next()

        let badRouted = ReplyEventOrderTests.Routed()
        let (closed, closing) = AsyncStream.makeStream(of: TransportCloseReason.self)
        bad.start(onEvent: { name, _, index in badRouted.record(name, index) },
                  onClose: { reason in
                      closing.yield(reason)
                      closing.finish()
                  })
        var closes = closed.makeAsyncIterator()
        let reason = await closes.next()
        #expect(reason == .lost("bad message: frames_not_negotiated"))
        #expect(badRouted.lines.withLock { $0.map(\.name) } == ["first"], "nothing after the bad byte routes")

        #expect(!good.isClosed)
        #expect(good.sendLine(Data("{}".utf8)))
        _ = await arrivals.next()
        #expect(routed.lines.withLock { $0.map(\.name) } == ["before", "after"])

        #expect(ledger.count("frames_not_negotiated") == 1)
        #expect(sunk.all.map(\.peer) == ["daemon terminal_attach"])
        #expect(sunk.all.map(\.reason) == ["frames_not_negotiated"])
    }

    /// A decoder above the transport that finds a bad message closes the connection through
    /// ``LineTransport/reportBadMessage(reason:detail:)``: it is reported, its requests fail, and the owner sees
    /// a lost connection.
    @Test func aDecoderReportedBadMessageClosesTheConnectionAsLost() async throws {
        let sunk = Sunk()
        let transport = try await LineTransport.open(
            path: Self.socketPath("decoder"), bridge: Self.bridge(#"b'{"event":"hello"}\n'"#),
            role: .control, stats: nil, badMessages: sunk.ledger())
        defer { transport.close() }
        let routed = ReplyEventOrderTests.Routed()
        let (closed, closing) = AsyncStream.makeStream(of: TransportCloseReason.self)
        transport.start(onEvent: { name, _, index in routed.record(name, index) },
                        onClose: { reason in
                            closing.yield(reason)
                            closing.finish()
                        })
        var arrivals = routed.arrivals.makeAsyncIterator()
        _ = await arrivals.next()

        transport.reportBadMessage(reason: "undecodable_event", detail: "hello")
        var closes = closed.makeAsyncIterator()
        let reason = await closes.next()
        #expect(reason == .lost("bad message: undecodable_event"))
        #expect(transport.isClosed)
        #expect(!transport.sendLine(Data("{}".utf8)))
        #expect(sunk.all == [.init(peer: "daemon control", reason: "undecodable_event", detail: "hello")])
    }

    /// A handler on the reader thread that reports a bad message stops the reader at once: the lines read in the
    /// same chunk after it reach no handler (Chrome's ReportBadMessage: nothing more from that peer is dispatched).
    @Test func nothingBufferedAfterAReportedBadMessageIsRouted() async throws {
        let sunk = Sunk()
        let transport = try await LineTransport.open(
            path: Self.socketPath("buffered"),
            bridge: Self.bridge(#"b'{"event":"bad"}\n{"event":"after"}\n{"ok":true,"id":1}\n'"#),
            role: .terminalAttach, stats: nil, badMessages: sunk.ledger())
        defer { transport.close() }
        let routed = ReplyEventOrderTests.Routed()
        let (closed, closing) = AsyncStream.makeStream(of: TransportCloseReason.self)
        transport.start(onEvent: { name, _, index in
                            routed.record(name, index)
                            if name == "bad" { transport.reportBadMessage(reason: "undecodable_attach_line", detail: name) }
                        },
                        onClose: { reason in
                            closing.yield(reason)
                            closing.finish()
                        })
        var closes = closed.makeAsyncIterator()
        let reason = await closes.next()
        #expect(reason == .lost("bad message: undecodable_attach_line"))
        #expect(routed.lines.withLock { $0.map(\.name) } == ["bad"], "the event after the bad one is not routed")
        #expect(sunk.all.map(\.reason) == ["undecodable_attach_line"])
    }

    /// The attach decoder's site: a known attach event that does not decode is reported with its event name
    /// (never its payload) and closes the attach connection as lost; the good output after it is never decoded.
    @Test func anUndecodableAttachLineClosesTheAttachConnection() async throws {
        let sunk = Sunk()
        let transport = try await LineTransport.open(
            path: Self.socketPath("attach"),
            bridge: Self.bridge(#"b'{"event":"output","surface":3,"data":12}\n{"event":"output","surface":3,"data":"YQ=="}\n'"#),
            role: .terminalAttach, stats: nil, badMessages: sunk.ledger())
        defer { transport.close() }
        let decoded = Mutex<[DecodedAttachLine?]>([])
        let (closed, closing) = AsyncStream.makeStream(of: TransportCloseReason.self)
        transport.start(onEvent: { name, line, _ in
                            let attachLine = TerminalAttachment.decodeAttachLine(name: name, line: line, surface: SurfaceID(rawValue: 3),
                                                                                 on: transport)
                            decoded.withLock { $0.append(attachLine) }
                        },
                        onClose: { reason in
                            closing.yield(reason)
                            closing.finish()
                        })
        var closes = closed.makeAsyncIterator()
        let reason = await closes.next()
        #expect(reason == .lost("bad message: undecodable_attach_line"))
        #expect(decoded.withLock { $0 } == [nil], "only the bad line was handled, and it decoded to nothing")
        #expect(sunk.all == [.init(peer: "daemon terminal_attach", reason: "undecodable_attach_line", detail: "output")])
    }
}
