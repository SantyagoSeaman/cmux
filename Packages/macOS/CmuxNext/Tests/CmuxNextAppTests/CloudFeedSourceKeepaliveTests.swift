@testable import CmuxNextFeed
import CmuxNextWakeups
import CmuxNextWakeupsTestSupport
import Foundation
import Testing

/// The feed socket stays silent while traffic flows (p14-energy-121): its keepalive probes only after
/// ``CloudFeedSource/keepaliveSilence`` with no frame either way, every answered probe counts as a
/// frame, and a sleep of the Mac or a stop drops the pending probe.
@MainActor
@Suite(.timeLimit(.minutes(1)))
struct CloudFeedSourceKeepaliveTests {
    private func source(_ clock: ManualClock, energy: EnergyPolicySource? = nil) -> CloudFeedSource {
        CloudFeedSource(apiBaseURL: URL(string: "https://api.test")!, device: "Mac", token: { "t" },
                        reconnect: Backoff(initial: .milliseconds(1), maximum: .milliseconds(1), jitter: 0),
                        connect: nil, clock: clock, energy: energy, keepaliveSilence: .seconds(15))
    }

    /// The probes so far, once the test ends the stream.
    private func count(_ probes: AsyncStream<Void>, ending probed: AsyncStream<Void>.Continuation) async -> Int {
        probed.finish()
        var count = 0
        for await _ in probes { count += 1 }
        return count
    }

    @Test func theShippingBudgetIsAMinute() {
        #expect(CloudFeedSource.keepaliveSilence == .seconds(60))
        let source = CloudFeedSource(apiBaseURL: URL(string: "https://api.test")!, device: "Mac") { "t" }
        #expect(source.keepalive.silence == CloudFeedSource.keepaliveSilence)
    }

    @Test func pongsEveryFiveSecondsWithAFifteenSecondBudgetSendNoProbe() async throws {
        let clock = ManualClock()
        let source = source(clock)
        let (probes, probed) = AsyncStream.makeStream(of: Void.self)
        source.keepalive.start { probed.yield() }
        for _ in 0..<12 {
            try await clock.waitForSleeper()
            clock.advance(by: .seconds(5))
            let answer = source.armPongLimit {}
            answer(false)
        }
        #expect(source.keepalive.isArmed)
        #expect(await count(probes, ending: probed) == 0)
    }

    @Test func sixteenSecondsOfSilenceSendOneProbe() async throws {
        let clock = ManualClock()
        let source = source(clock)
        let (probes, probed) = AsyncStream.makeStream(of: Void.self)
        source.keepalive.start { probed.yield() }
        var fired = probes.makeAsyncIterator()
        try await clock.waitForSleeper()
        clock.advance(by: .seconds(16))
        _ = await fired.next()
        #expect(source.keepalive.isArmed)
    }

    @Test func aSleepOfTheMacAndAStopDropThePendingProbe() async throws {
        let clock = ManualClock()
        let energy = EnergyPolicySource()
        let source = source(clock, energy: energy)
        source.keepalive.start {}
        #expect(source.keepalive.isArmed)
        energy.update(EnergyPolicy(suspended: true))
        #expect(!source.keepalive.isArmed)
        source.keepalive.heard()
        #expect(!source.keepalive.isArmed)
        energy.update(EnergyPolicy())
        source.keepalive.heard()
        #expect(source.keepalive.isArmed)
        source.stop()
        #expect(!source.keepalive.isArmed)
    }
}
