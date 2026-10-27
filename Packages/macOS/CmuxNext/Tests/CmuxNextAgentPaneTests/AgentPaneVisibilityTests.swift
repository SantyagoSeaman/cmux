import CmuxNextCompat
import Foundation
import Testing
@testable import CmuxNextAgentPane

/// p14-energy-118: the pane tells acpmux each seen/unseen edge of the chat it shows, once, with one
/// fire-and-forget `_acpmux/visibility` notification sent off the main thread after the caller's turn.
@Suite
@MainActor
struct AgentPaneVisibilityTests {
    /// A wire that records each frame the host sent, the thread it went on, and wakes a waiter per frame.
    /// While it holds, a send's completion waits for ``release()`` (the send stays outstanding).
    /// Nonisolated: the socket sends on its own queue.
    nonisolated final class Wire: AcpmuxPaneWire, @unchecked Sendable { // crash-allow: test support; every access holds the lock
        struct Sent: Sendable {
            let text: String
            let onMain: Bool
        }

        private struct State {
            var sent: [Sent] = []
            var waiters: [(Int, CheckedContinuation<[Sent], Never>)] = []
            var holds = false
            var held: [@Sendable (Bool) -> Void] = []
        }

        private let state = Mutex(State())

        func open(onFrame: @escaping @Sendable (String) -> Void, onClose: @escaping @Sendable (Int, String) -> Void) async throws {}

        func send(_ text: String, completion: @escaping @Sendable (Bool) -> Void) {
            let (ready, holds) = state.withLock { state -> ([(CheckedContinuation<[Sent], Never>, [Sent])], Bool) in
                state.sent.append(Sent(text: text, onMain: Thread.isMainThread))
                if state.holds { state.held.append(completion) }
                var ready: [(CheckedContinuation<[Sent], Never>, [Sent])] = []
                let sent = state.sent
                state.waiters.removeAll { count, waiter in
                    guard sent.count >= count else { return false }
                    ready.append((waiter, sent))
                    return true
                }
                return (ready, state.holds)
            }
            if !holds { completion(true) }
            for (waiter, sent) in ready { waiter.resume(returning: sent) }
        }

        func cancel(code: Int, reason: String) {}

        var frames: [Sent] { state.withLock { $0.sent } }

        /// Holds every send's completion from now on, until ``release()``.
        func hold() { state.withLock { $0.holds = true } }

        /// Completes the held sends and stops holding.
        func release() {
            let held = state.withLock { state -> [@Sendable (Bool) -> Void] in
                state.holds = false
                defer { state.held = [] }
                return state.held
            }
            for completion in held { completion(true) }
        }

        /// The frames sent once there are at least `count`.
        func sent(count: Int) async -> [Sent] {
            await withCheckedContinuation { waiter in
                let now = state.withLock { state -> [Sent]? in
                    if state.sent.count >= count { return state.sent }
                    state.waiters.append((count, waiter))
                    return nil
                }
                if let now { waiter.resume(returning: now) }
            }
        }
    }

    static func socket(_ wire: Wire, limits: AgentPaneTransport.Limits = AgentPaneTransport.Limits()) throws -> AcpmuxPaneSocket {
        let url = try #require(URL(string: "ws://127.0.0.1:9/acp"))
        return AcpmuxPaneSocket(request: URLRequest(url: url), wire: wire, limits: limits,
                                options: AcpmuxPermissionOptions(), sessions: AcpmuxPaneSessions(), ids: AcpmuxRequestIds()) {}
    }

    /// The `(sessionId, visible)` of a `_acpmux/visibility` notification; nil for any other frame.
    static func visibility(_ text: String) -> (String, Bool)? {
        guard let object = (try? JSONSerialization.jsonObject(with: Data(text.utf8))) as? [String: Any],
              object["method"] as? String == "_acpmux/visibility", object["id"] == nil,
              let params = object["params"] as? [String: Any], let session = params["sessionId"] as? String,
              let visible = params["visible"] as? Bool else { return nil }
        return (session, visible)
    }

    /// Returns once every state handed to the socket's outbound queue so far was made and sent (or refused).
    static func outboundDone(_ socket: AcpmuxPaneSocket) async {
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in socket.outbound.async { done.resume() } }
    }

    @Test func hideHideShowSendsExactlyTwoNotificationsOffTheMainThread() async throws {
        let wire = Wire()
        let socket = try Self.socket(wire)
        try await socket.start(timeout: 1)

        // Synchronous on the main actor: the edge never awaits in its turn.
        socket.visibilityChanged(session: "s", visible: false)
        socket.visibilityChanged(session: "s", visible: false)
        socket.visibilityChanged(session: "s", visible: true)

        // The queue is serial and the repeat was never queued: once the show went, nothing else follows.
        await Self.outboundDone(socket)
        let sent = wire.frames
        let edges = sent.compactMap { Self.visibility($0.text) }
        #expect(sent.count == 2, "\(sent.map(\.text))")
        #expect(edges.map { $0.0 } == ["s", "s"])
        #expect(edges.map { $0.1 } == [false, true])
        #expect(sent.allSatisfy { !$0.onMain }, "a visibility frame was made and sent in the caller's main-thread turn")
        socket.close(code: 1000, reason: "", error: nil)
    }

    @Test func anotherSessionsEdgeIsItsOwn() async throws {
        let wire = Wire()
        let socket = try Self.socket(wire)
        try await socket.start(timeout: 1)

        socket.visibilityChanged(session: "a", visible: false)
        socket.visibilityChanged(session: "b", visible: false)
        // Each session keeps its own last state: a's repeat after b's edge sends nothing.
        socket.visibilityChanged(session: "a", visible: false)
        socket.visibilityChanged(session: "end", visible: true)

        await Self.outboundDone(socket)
        let edges = wire.frames.compactMap { Self.visibility($0.text) }
        #expect(edges.map { $0.0 } == ["a", "b", "end"])
        #expect(edges.map { $0.1 } == [false, false, true])
        socket.close(code: 1000, reason: "", error: nil)
    }

    @Test func aSessionTheConnectionLeavesIsHiddenOnlyIfItWasVisible() async throws {
        let wire = Wire()
        let socket = try Self.socket(wire)
        try await socket.start(timeout: 1)

        socket.sessionLeft("never")
        socket.visibilityChanged(session: "hidden", visible: false)
        socket.sessionLeft("hidden")
        socket.visibilityChanged(session: "shown", visible: true)
        socket.sessionLeft("shown")
        socket.sessionLeft("shown")
        socket.visibilityChanged(session: "end", visible: true)

        await Self.outboundDone(socket)
        let edges = wire.frames.compactMap { Self.visibility($0.text) }
        #expect(edges.map { $0.0 } == ["hidden", "shown", "shown", "end"])
        #expect(edges.map { $0.1 } == [false, true, false, true])
        socket.close(code: 1000, reason: "", error: nil)
    }

    /// A refused send (the outbound bound is full) is not acpmux's state: the same edge goes again later.
    @Test func aRefusedEdgeIsNotRecordedAsSent() async throws {
        let wire = Wire()
        var limits = AgentPaneTransport.Limits()
        limits.maximumOutstandingSends = 1
        let socket = try Self.socket(wire, limits: limits)
        try await socket.start(timeout: 1)

        wire.hold()
        socket.visibilityChanged(session: "s", visible: false)
        await Self.outboundDone(socket)
        // The hide is still outstanding: the show is refused for the bound.
        socket.visibilityChanged(session: "s", visible: true)
        await Self.outboundDone(socket)
        #expect(wire.frames.count == 1)

        wire.release()
        socket.visibilityChanged(session: "s", visible: true)
        socket.visibilityChanged(session: "end", visible: true)
        await Self.outboundDone(socket)
        let edges = wire.frames.compactMap { Self.visibility($0.text) }
        #expect(edges.map { $0.0 } == ["s", "s", "end"], "the refused show was kept as sent, so it never went again")
        #expect(edges.map { $0.1 } == [false, true, true])
        socket.close(code: 1000, reason: "", error: nil)
    }

    @Test func aSocketNotOpenOrClosedSendsNothing() async throws {
        let wire = Wire()
        let socket = try Self.socket(wire)
        socket.visibilityChanged(session: "s", visible: false)
        try await socket.start(timeout: 1)
        socket.close(code: 1000, reason: "", error: nil)
        socket.visibilityChanged(session: "s", visible: true)
        await Self.outboundDone(socket)
        #expect(wire.frames.isEmpty, "\(wire.frames.map(\.text))")
    }
}
