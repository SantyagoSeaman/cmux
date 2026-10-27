import CmuxNextWakeups
import CmuxNextWakeupsTestSupport
import Testing

/// A client socket's keepalive runs only after a silence budget with no frame either way
/// (p14-energy-121): traffic every 5 s with a 15 s budget sends no probe, an idle socket gets one
/// probe per budget, and a stopped or paused keepalive sends none.
@MainActor
@Suite("SilenceKeepalive")
struct SilenceKeepaliveTests {
    /// A keepalive on a manual clock with a 15 s budget, and its probes as a stream.
    @MainActor
    private struct Rig {
        let clock = ManualClock()
        let keepalive: SilenceKeepalive
        let probes: AsyncStream<Void>
        let probed: AsyncStream<Void>.Continuation

        init() {
            keepalive = SilenceKeepalive(owner: "test.silence-keepalive", silence: .seconds(15), clock: clock)
            let stream = AsyncStream.makeStream(of: Void.self)
            probes = stream.stream
            probed = stream.continuation
        }

        func start() {
            let probed = probed
            keepalive.start { probed.yield() }
        }

        /// The probes so far; ends the stream (every probe has run once its fire was awaited).
        func count() async -> Int {
            probed.finish()
            var count = 0
            for await _ in probes { count += 1 }
            return count
        }
    }

    @Test(.timeLimit(.minutes(1))) func trafficEveryFiveSecondsWithAFifteenSecondBudgetSendsNoProbe() async throws {
        let rig = Rig()
        rig.start()
        for _ in 0..<12 {
            try await rig.clock.waitForSleeper()
            rig.clock.advance(by: .seconds(5))
            rig.keepalive.heard()
        }
        try await rig.clock.waitForSleeper()
        #expect(rig.keepalive.isArmed)
        #expect(await rig.count() == 0)
    }

    @Test(.timeLimit(.minutes(1))) func anIdleSocketGetsOneProbePerBudget() async throws {
        let rig = Rig()
        rig.start()
        var probes = rig.probes.makeAsyncIterator()
        try await rig.clock.waitForSleeper()
        #expect(rig.clock.sleepRequests.last?.deadline.offset == .seconds(15))
        rig.clock.advance(by: .seconds(14))
        #expect(rig.clock.sleeperCount == 1)
        rig.clock.advance(by: .seconds(2))
        _ = await probes.next()
        try await rig.clock.waitForSleeper()
        rig.clock.advance(by: .seconds(15))
        _ = await probes.next()
        #expect(rig.keepalive.isArmed)
    }

    @Test(.timeLimit(.minutes(1))) func stopDropsTheDeadlineAndNoProbeRuns() async throws {
        let rig = Rig()
        rig.start()
        try await rig.clock.waitForSleeper()
        rig.keepalive.stop()
        #expect(!rig.keepalive.isArmed)
        rig.clock.advance(by: .seconds(60))
        #expect(await rig.count() == 0)
    }

    @Test(.timeLimit(.minutes(1))) func aFrameOrPongAfterStopArmsNothing() async throws {
        let rig = Rig()
        rig.keepalive.heard()
        #expect(!rig.keepalive.isArmed)
        rig.start()
        try await rig.clock.waitForSleeper()
        rig.keepalive.stop()
        // A pong of a ping that was in flight when the connection closed.
        rig.keepalive.heard()
        #expect(!rig.keepalive.isArmed)
        rig.clock.advance(by: .seconds(60))
        #expect(await rig.count() == 0)
    }

    @Test(.timeLimit(.minutes(1))) func pauseDropsTheDeadlineUntilTheNextFrame() async throws {
        let rig = Rig()
        rig.start()
        try await rig.clock.waitForSleeper()
        rig.keepalive.pause()
        #expect(!rig.keepalive.isArmed)
        rig.clock.advance(by: .seconds(60))
        rig.keepalive.heard()
        #expect(rig.keepalive.isArmed)
        var probes = rig.probes.makeAsyncIterator()
        try await rig.clock.waitForSleeper()
        rig.clock.advance(by: .seconds(15))
        _ = await probes.next()
    }
}
