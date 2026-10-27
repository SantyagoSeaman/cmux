/// A client socket's keepalive that fires only after ``silence`` with no frame either way
/// (p14-energy-121). Every frame the owner reads or writes calls ``heard()``, which pushes the one
/// deadline back; while frames flow the probe never runs, and an idle socket gets one probe per
/// ``silence``. Chromium checks a connection the same way: its HTTP/2 session sends the connection
/// check PING only after a read silence (net/spdy/spdy_session.cc, `SendPrefacePingIfNoneInFlight`),
/// never on a fixed period.
///
/// The probe is the owner's: an RFC 6455 ping with its own pong deadline, which a Cloudflare Durable
/// Object answers without waking (hibernation), so the server never needs to ping an idle client.
/// With a ``KeepaliveGate`` nothing is armed while the gate holds the keepalive; the owner's pause
/// handler calls ``pause()`` and its resume hook calls ``heard()``.
@MainActor
public final class SilenceKeepalive {
    /// How long the socket stays silent, both ways, before the probe runs.
    public nonisolated let silence: Duration
    private nonisolated let timer: DemandTimer
    private nonisolated let gate: KeepaliveGate?
    private var probe: (@MainActor () -> Void)?

    /// - Parameters:
    ///   - owner: The ledger name of each probe (the `allowed_periodic` owner in idle-budget.json).
    ///   - silence: The silence budget before a probe; a flexible deadline (``WakeLeeway``: 10% from 1 s on).
    ///   - clock: The clock the deadline sleeps on; tests pass a `ManualClock`.
    ///   - gate: The owner's Energy Saver gate; nil never holds the keepalive.
    public nonisolated init(owner: String, silence: Duration, clock: any Clock<Duration> = ContinuousClock(),
                            gate: KeepaliveGate? = nil) {
        self.silence = silence
        self.gate = gate
        timer = DemandTimer(owner: owner, clock: clock)
    }

    /// True while a probe deadline is pending.
    public nonisolated var isArmed: Bool { timer.isScheduled }

    /// A connection opened: `probe` runs after each ``silence`` with no ``heard()``, until ``stop()``.
    public func start(_ probe: @escaping @MainActor () -> Void) {
        self.probe = probe
        heard()
    }

    /// A frame went in or out: the probe waits a full ``silence`` from now. Safe from any thread.
    public nonisolated func heard() {
        let arm = { [timer, silence] in
            // wakeup-allow: deadline: pushed back by every frame; it fires only after a full silence budget.
            timer.scheduleOnMain(after: silence) { [weak self] in self?.fire() }
        }
        if let gate { gate.whileAwake(arm) } else { arm() }
    }

    /// The gate holds the keepalive: drops the deadline until the next ``heard()``. Safe from any thread.
    public nonisolated func pause() {
        timer.cancel()
    }

    /// The connection closed: no probe runs again until the next ``start(_:)``.
    public func stop() {
        probe = nil
        timer.cancel()
    }

    /// The silence budget passed: runs the probe and waits another full budget, so an idle socket gets
    /// one probe per ``silence``.
    private func fire() {
        guard let probe else { return }
        probe()
        heard()
    }
}
