import CmuxNextCompat
import os

/// The app's crash-loop breaker of framed IPC (p05-ipc-191; plans/cmux-next/chrome-parity/p05-ipc.md 3.7).
///
/// A bad message from the daemon closes its connection (``LineTransport``). When the connection had asked for
/// frames, the close also counts here, by daemon life (its `identify` pid and boot generation); a close inside a
/// frame is a lost connection (the daemon died or dropped a slow reader mid-write) and counts nothing. The third such
/// close of one life within ``tripWindow`` turns frames off for that life. Every later handshake with it asks
/// for no frames (``WireNegotiation``), so the app goes on with JSON lines instead of reconnecting into the
/// same bad frame forever. A new daemon life starts with frames on again. Chrome ends a renderer that sends a
/// bad message once and shows a sad tab, with no reconnect loop (content/browser/bad_message.cc,
/// `ReceivedBadMessage`); here the connection comes back, so the breaker ends the loop instead.
///
/// The trip is visible four ways: the close reason of the connection that tripped it, which becomes the
/// connection's `disconnected` state (``tripDetail``); an error in the unified log; the ``DaemonWireStats``
/// counter `frameBreakerTrips` (`debug.daemon.wire`); and a ``Trip`` on ``trips``, held until a sink is
/// installed: the app's `WatchdogReporting` sends it on as a crash-report event (`BlockingReportSink`).
///
/// One breaker serves every connection to one daemon: the owner of that daemon target (`DaemonService`) makes
/// it once and passes it into every ``DaemonWireUpgrades`` that asks for frames.
public final class DaemonFrameBreaker: Sendable {
    /// Bad-message closes of one daemon life's framed connections that turn its frames off.
    public static let tripCloses = 3
    /// The window those closes fall in.
    public static let tripWindow: Duration = .seconds(60)
    /// What the close that trips the breaker adds to its connection's close reason, which the app shows as the
    /// daemon connection's state.
    static let tripDetail = "framed IPC off for this daemon (\(tripCloses) bad messages within 60 s); JSON lines from now on"

    /// Frames turned off for one daemon life.
    public struct Trip: Sendable, Identifiable, Equatable {
        /// The life (``DaemonFrameBreaker/life(of:)``); one trip per life.
        public let daemonLife: String
        /// The bad-message reasons of the closes that tripped it, oldest first.
        public let reasons: [String]

        public var id: String { daemonLife }
    }

    /// What one counted close did.
    enum Verdict: Equatable {
        /// Counted; frames stay on for the life.
        case counted
        /// This close turned frames off for the life.
        case tripped
        /// Frames were off for the life already.
        case alreadyOff
    }

    /// A framed connection's link to the breaker: it reports its bad-message close under its daemon's life.
    struct Watch: Sendable {
        let breaker: DaemonFrameBreaker
        let life: String

        func badMessage(_ reason: String) -> Verdict { breaker.record(reason, life: life) }
    }

    private struct Close {
        var at: Duration
        var reason: String
    }

    private struct State {
        /// The last ``tripCloses`` closes of each life with a close inside the window.
        var closes: [String: [Close]] = [:]
        /// The lives whose frames are off.
        var tripped: Set<String> = []
    }

    /// The trips, each once, for the app's diagnostics sink (held until it is installed).
    public let trips = DaemonDiagnosticSiteRelay<Trip>()

    // Lock carve-out: taken once per bad-message close of a framed connection and once per handshake that
    // could ask for frames; it guards two small collections and no call runs under it.
    private let state = Mutex(State())
    private let elapsed: @Sendable () -> Duration
    private let logger = Logger(subsystem: "com.cmuxterm.app.next", category: "daemon")

    /// - Parameter clock: What the window is measured on; tests pass a `ManualClock`.
    public init(clock: any Clock<Duration> = ContinuousClock()) {
        elapsed = LineTransport.elapsedReader(clock)
    }

    /// The daemon life `identity` names: its pid and boot generation, as diagnostic sites name it.
    static func life(of identity: DaemonIdentity) -> String {
        DaemonDiagnosticSite.Sighting.life(pid: identity.pid, generation: identity.generation.rawValue)
    }

    /// Whether frames are off for `identity`'s daemon life.
    func framesOff(for identity: DaemonIdentity) -> Bool {
        let life = Self.life(of: identity)
        return state.withLock { $0.tripped.contains(life) }
    }

    /// The watch a connection to `identity`'s daemon that asks for frames reports its bad message to.
    func watch(_ identity: DaemonIdentity) -> Watch {
        Watch(breaker: self, life: Self.life(of: identity))
    }

    /// Counts one bad-message close of a framed connection to `life`. The close that trips the breaker logs
    /// the trip and hands it to ``trips``.
    func record(_ reason: String, life: String) -> Verdict {
        let now = elapsed()
        let window = Self.tripWindow
        let (verdict, trip) = state.withLock { state -> (Verdict, Trip?) in
            if state.tripped.contains(life) { return (.alreadyOff, nil) }
            // A life with no close inside the window is forgotten: daemon restarts leave no entries behind.
            state.closes = state.closes.filter { _, closes in closes.last.map { now - $0.at <= window } ?? false }
            var closes = state.closes[life, default: []]
            closes.append(Close(at: now, reason: reason))
            closes = Array(closes.filter { now - $0.at <= window }.suffix(Self.tripCloses))
            guard closes.count == Self.tripCloses else {
                state.closes[life] = closes
                return (.counted, nil)
            }
            state.closes[life] = nil
            state.tripped.insert(life)
            return (.tripped, Trip(daemonLife: life, reasons: closes.map(\.reason)))
        }
        if let trip {
            let reasons = trip.reasons.joined(separator: ", ")
            let closes = Self.tripCloses, seconds = Self.tripWindow.components.seconds
            logger.error("framed IPC off for daemon \(life, privacy: .public): \(closes) bad-message closes within \(seconds) s (\(reasons, privacy: .public)); JSON lines from now on")
            trips.deliver(trip)
        }
        return verdict
    }
}
