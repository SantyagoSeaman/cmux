@testable import CmuxNextCloud
import CmuxNextWakeups
import CmuxNextWakeupsTestSupport
import Foundation
import Testing

/// The user socket stays silent while traffic flows (p14-energy-121): its RFC 6455 ping goes out only
/// after ``HandApprovalWire/keepaliveSilence`` with no frame either way; the pong re-arms it, and no
/// pong within ``HandApprovalWire/pongLimit`` closes the socket.
@MainActor
@Suite(.timeLimit(.minutes(1)))
struct HandApprovalWireKeepaliveTests {
    /// A socket that records pings and closes; `pong` answers each ping (nil never answers).
    private final class FakeSocket: HandKeepaliveSocket {
        enum Event: Sendable, Equatable { case ping, close }
        let events: AsyncStream<Event>
        private let event: AsyncStream<Event>.Continuation
        private let pong: Bool?

        init(pong: Bool?) {
            let stream = AsyncStream.makeStream(of: Event.self)
            events = stream.stream
            event = stream.continuation
            self.pong = pong
        }

        func keepalivePing(_ answered: @escaping @Sendable (_ failed: Bool) -> Void) {
            event.yield(.ping)
            if let pong { answered(!pong) }
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

    private func wire(_ clock: ManualClock) -> HandApprovalWire {
        HandApprovalWire(apiBaseURL: URL(string: "https://api.test")!, clock: clock, keepaliveSilence: .seconds(15)) { "t" }
    }

    @Test func theShippingBudgetIsAMinute() {
        #expect(HandApprovalWire.keepaliveSilence == .seconds(60))
        let wire = HandApprovalWire(apiBaseURL: URL(string: "https://api.test")!) { "t" }
        #expect(wire.keepalive.silence == HandApprovalWire.keepaliveSilence)
    }

    @Test func framesEveryFiveSecondsWithAFifteenSecondBudgetSendNoPing() async throws {
        let clock = ManualClock()
        let wire = wire(clock)
        let socket = FakeSocket(pong: true)
        wire.keepalive.start { wire.ping(socket) }
        for _ in 0..<12 {
            try await clock.waitForSleeper()
            clock.advance(by: .seconds(5))
            wire.heard()
        }
        #expect(wire.keepalive.isArmed)
        #expect(await socket.recorded().isEmpty)
    }

    @Test func sixteenSecondsOfSilenceSendOnePingAndItsPongKeepsTheSocket() async throws {
        let clock = ManualClock()
        let wire = wire(clock)
        let socket = FakeSocket(pong: true)
        wire.keepalive.start { wire.ping(socket) }
        var events = socket.events.makeAsyncIterator()
        try await clock.waitForSleeper()
        clock.advance(by: .seconds(16))
        #expect(await events.next() == .ping)
        // The pong dropped its deadline: the only sleeper left is the next silence budget.
        try await clock.waitForSleeper()
        clock.advance(by: HandApprovalWire.pongLimit)
        #expect(await socket.recorded().isEmpty)
    }

    @Test func noPongWithinTheLimitClosesTheSocket() async throws {
        let clock = ManualClock()
        let wire = wire(clock)
        let socket = FakeSocket(pong: nil)
        var events = socket.events.makeAsyncIterator()
        wire.ping(socket)
        #expect(await events.next() == .ping)
        try await clock.waitForSleeper()
        clock.advance(by: HandApprovalWire.pongLimit)
        #expect(await events.next() == .close)
    }

    @Test func aFailedPingClosesAtOnce() async {
        let clock = ManualClock()
        let wire = wire(clock)
        let socket = FakeSocket(pong: false)
        wire.ping(socket)
        #expect(await socket.recorded() == [.ping, .close])
    }

    @Test func stopDropsTheKeepaliveAndThePongDeadline() async throws {
        let clock = ManualClock()
        let wire = wire(clock)
        let socket = FakeSocket(pong: nil)
        wire.keepalive.start { wire.ping(socket) }
        wire.ping(socket)
        try await clock.waitForSleeper()
        wire.stop()
        #expect(!wire.keepalive.isArmed)
        clock.advance(by: .seconds(60))
        #expect(await socket.recorded() == [.ping])
    }
}
