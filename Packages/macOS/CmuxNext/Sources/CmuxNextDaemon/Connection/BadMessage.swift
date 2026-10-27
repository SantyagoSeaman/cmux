import CmuxNextCompat
import os

/// The app end of Chrome's `ReportBadMessage` (mojo/public/cpp/bindings/message.h,
/// content/browser/bad_message.cc): a daemon connection whose peer broke the wire's rules is closed, and only
/// that connection; ``report(peer:reason:detail:)`` counts the reason and logs its first report of the launch
/// (Chrome counts `Stability.BadMessageTerminated.*` and takes one dump without crashing). The connection's
/// owner then reconnects as after any lost connection. The daemon's own ledger is cmux-panic-log
/// `bad_message.rs`; the reasons are the wire's (`BadMessage.reason` of CmuxNextIPC) or a decoder's.
///
/// One ledger serves one daemon for the launch: its ``DaemonLauncher`` (the local daemon) or its remote machine's
/// service makes it and puts it on every endpoint it hands out (``DaemonEndpoint/badMessages``), so each
/// connection's ``LineTransport`` reports there; tests pass their own. Reports are rare (each one ends a
/// connection), so the counts sit under one ``Mutex``.
public final class BadMessageReports: Sendable {
    /// The first report of one reason.
    public struct Report: Sendable, Equatable {
        /// The connection that sent it: its role (``LineTransport/badMessagePeer``).
        public var peer: String
        public var reason: String
        /// The wire fault (lengths, kinds, offsets), never message content.
        public var detail: String
    }

    /// Where each reason's first report goes.
    public typealias Sink = @Sendable (Report) -> Void

    /// Whether a report was its reason's first in this ledger.
    enum Reported: Sendable, Equatable {
        /// The first of its reason: it went to the sink.
        case first
        /// Counted only; the reason was reported before.
        case repeated
    }

    /// Logs a report as a fault: it reaches the unified log and a sysdiagnose.
    public static let log: Sink = { report in
        let (reason, peer, detail) = (report.reason, report.peer, report.detail)
        Logger(subsystem: "com.cmuxterm.app.next", category: "daemon.bad-message").fault(
            "bad_message \(reason, privacy: .public) from \(peer, privacy: .public): \(detail, privacy: .public)")
    }

    private let counts = Mutex<[String: UInt64]>([:])
    private let sink: Sink

    public init(sink: @escaping Sink = BadMessageReports.log) {
        self.sink = sink
    }

    /// `peer` sent a bad message: counts `reason` and sends its first report of this ledger to the sink. The
    /// caller closes that peer's connection, and only it.
    @discardableResult
    func report(peer: String, reason: String, detail: String) -> Reported {
        let before = counts.withLock { counts -> UInt64 in
            let before = counts[reason, default: 0]
            counts[reason] = before &+ 1
            return before
        }
        guard before == 0 else { return .repeated }
        sink(Report(peer: peer, reason: reason, detail: detail))
        return .first
    }

    /// Reports of `reason` so far.
    public func count(_ reason: String) -> UInt64 {
        counts.withLock { $0[reason, default: 0] }
    }

    /// Every reason reported so far with its count.
    public var all: [String: UInt64] { counts.withLock { $0 } }
}
