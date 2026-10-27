@testable import CmuxNextCloud
import CmuxNextWakeups
import CmuxNextWakeupsTestSupport
import Foundation
import Testing

/// The user socket stays silent while traffic flows (p14-energy-121): its RFC 6455 ping goes out only
/// after ``HandApprovalWire/keepaliveSilence`` with no frame either way; the pong re-arms it, no pong
/// within ``HandApprovalWire/pongLimit`` closes the socket, and a sleep of the Mac or its displays
/// (``KeepaliveGate``) or a stop drops the armed keepalive and the pong deadline.
@MainActor
@Suite(.timeLimit(.minutes(1)))
struct HandApprovalWireKeepaliveTests {
    /// A socket that records pings and closes; `pong` answers each ping at once (nil leaves the answer
    /// to the test, through ``answers``).
    private nonisolated final class FakeSocket: HandKeepaliveSocket {
        enum Event: Sendable, Equatable { case ping, close }
        typealias Answer = @Sendable (_ failed: Bool) -> Void
        let events: AsyncStream<Event>
        /// The answer of each ping that `pong` nil left unanswered.
        let answers: AsyncStream<Answer>
        private let event: AsyncStream<Event>.Continuation
        private let answer: AsyncStream<Answer>.Continuation
        private let pong: Bool?

        init(pong: Bool?) {
            let stream = AsyncStream.makeStream(of: Event.self)
            events = stream.stream
            event = stream.continuation
            let answerStream = AsyncStream.makeStream(of: Answer.self)
            answers = answerStream.stream
            answer = answerStream.continuation
            self.pong = pong
        }

        func keepalivePing(_ answered: @escaping @Sendable (_ failed: Bool) -> Void) {
            event.yield(.ping)
            if let pong { answered(!pong) } else { answer.yield(answered) }
        }

        func keepaliveClose() { event.yield(.close) }

        /// Every event so far, once the test ends the stream.
        func recorded() async -> [Event] {
            event.finish()
            var all: [Event] = []
            for await next in events { all.append(next) }
            return all
        }
    }

    private func makeWire(_ clock: ManualClock, energy: EnergyPolicySource? = nil) -> HandApprovalWire {
        HandApprovalWire(apiBaseURL: URL(string: "https://api.test")!, clock: clock, energy: energy,
                         keepaliveSilence: .seconds(15), wakeJitter: { 0 }) { "t" }
    }

    @Test func theShippingBudgetIsAMinute() {
        #expect(HandApprovalWire.keepaliveSilence == .seconds(60))
        let wire = HandApprovalWire(apiBaseURL: URL(string: "https://api.test")!) { "t" }
        #expect(wire.keepalive.silence == HandApprovalWire.keepaliveSilence)
    }

    @Test func framesEveryFiveSecondsWithAFifteenSecondBudgetSendNoPing() async throws {
        let clock = ManualClock()
        let wire = makeWire(clock)
        let socket = FakeSocket(pong: true)
        wire.keepalive.start { wire.ping(socket) }
        for _ in 0..<12 {
            try await clock.waitForSleeper()
            clock.advance(by: .seconds(5))
            wire.heard()
        }
        #expect(wire.keepalive.isArmed)
        #expect(!wire.awaitsPong)
        #expect(await socket.recorded().isEmpty)
    }

    @Test func sixteenSecondsOfSilenceSendOnePingAndItsPongDropsTheDeadline() async throws {
        let clock = ManualClock()
        let wire = makeWire(clock)
        let socket = FakeSocket(pong: true)
        wire.keepalive.start { wire.ping(socket) }
        var events = socket.events.makeAsyncIterator()
        try await clock.waitForSleeper()
        clock.advance(by: .seconds(16))
        #expect(await events.next() == .ping)
        // The pong dropped the deadline and re-armed the silence budget.
        #expect(!wire.awaitsPong)
        #expect(wire.keepalive.isArmed)
    }

    @Test func noPongWithinTheLimitClosesTheSocket() async throws {
        let clock = ManualClock()
        let wire = makeWire(clock)
        let socket = FakeSocket(pong: nil)
        var events = socket.events.makeAsyncIterator()
        wire.ping(socket)
        #expect(await events.next() == .ping)
        #expect(wire.awaitsPong)
        try await clock.waitForSleeper()
        clock.advance(by: HandApprovalWire.pongLimit)
        #expect(await events.next() == .close)
    }

    @Test func aFailedPingClosesAtOnce() async {
        let clock = ManualClock()
        let wire = makeWire(clock)
        let socket = FakeSocket(pong: false)
        wire.ping(socket)
        #expect(!wire.awaitsPong)
        #expect(await socket.recorded() == [.ping, .close])
    }

    @Test func stopDropsTheKeepaliveAndThePongDeadline() async throws {
        let clock = ManualClock()
        let wire = makeWire(clock)
        let socket = FakeSocket(pong: nil)
        wire.keepalive.start { wire.ping(socket) }
        wire.ping(socket)
        #expect(wire.awaitsPong)
        wire.stop()
        #expect(!wire.keepalive.isArmed)
        #expect(!wire.awaitsPong)
    }

    @Test func aPongThatLandsAfterStopArmsNothing() async throws {
        let clock = ManualClock()
        let wire = makeWire(clock)
        let socket = FakeSocket(pong: nil)
        var answers = socket.answers.makeAsyncIterator()
        wire.keepalive.start { wire.ping(socket) }
        wire.ping(socket)
        let answer = try #require(await answers.next())
        wire.stop()
        answer(false)
        #expect(!wire.keepalive.isArmed)
        #expect(!wire.awaitsPong)
    }

    @Test func aSleepOfTheMacOrItsDisplaysDropsTheKeepaliveAndThePongDeadline() async throws {
        for asleep in [EnergyPolicy(suspended: true), EnergyPolicy(screensAsleep: true)] {
            let clock = ManualClock()
            let energy = EnergyPolicySource()
            let wire = makeWire(clock, energy: energy)
            let socket = FakeSocket(pong: nil)
            wire.keepalive.start { wire.ping(socket) }
            wire.ping(socket)
            #expect(wire.keepalive.isArmed)
            #expect(wire.awaitsPong)
            energy.update(asleep)
            #expect(!wire.keepalive.isArmed)
            #expect(!wire.awaitsPong)
            // A frame while the gate holds arms nothing; once awake it arms again.
            wire.heard()
            #expect(!wire.keepalive.isArmed)
            energy.update(EnergyPolicy())
            wire.heard()
            #expect(wire.keepalive.isArmed)
            wire.stop()
        }
    }
}
