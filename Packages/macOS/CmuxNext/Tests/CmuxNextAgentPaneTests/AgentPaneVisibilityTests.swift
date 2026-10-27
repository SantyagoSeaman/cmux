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
    /// Nonisolated: the socket sends on its own queue.
    nonisolated final class Wire: AcpmuxPaneWire, @unchecked Sendable { // crash-allow: test support; every access holds the lock
        struct Sent: Sendable {
            let text: String
            let onMain: Bool
        }

        private let state = Mutex<(sent: [Sent], waiters: [(Int, CheckedContinuation<[Sent], Never>)])>(([], []))

        func open(onFrame: @escaping @Sendable (String) -> Void, onClose: @escaping @Sendable (Int, String) -> Void) async throws {}

        func send(_ text: String, completion: @escaping @Sendable (Bool) -> Void) {
            let ready = state.withLock { state -> [(CheckedContinuation<[Sent], Never>, [Sent])] in
                state.sent.append(Sent(text: text, onMain: Thread.isMainThread))
                var ready: [(CheckedContinuation<[Sent], Never>, [Sent])] = []
                let sent = state.sent
                state.waiters.removeAll { count, waiter in
                    guard sent.count >= count else { return false }
                    ready.append((waiter, sent))
                    return true
                }
                return ready
            }
            completion(true)
            for (waiter, sent) in ready { waiter.resume(returning: sent) }
        }

        func cancel(code: Int, reason: String) {}

        var frames: [Sent] { state.withLock { $0.sent } }

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

    static func socket(_ wire: Wire) throws -> AcpmuxPaneSocket {
        let url = try #require(URL(string: "ws://127.0.0.1:9/acp"))
        return AcpmuxPaneSocket(request: URLRequest(url: url), wire: wire, limits: AgentPaneTransport.Limits(),
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

    @Test func hideHideShowSendsExactlyTwoNotificationsOffTheMainThread() async throws {
        let wire = Wire()
        let socket = try Self.socket(wire)
        try await socket.start(timeout: 1)

        // Synchronous on the main actor: the edge never awaits in its turn.
        socket.visibilityChanged(session: "s", visible: false)
        socket.visibilityChanged(session: "s", visible: false)
        socket.visibilityChanged(session: "s", visible: true)

        // The queue is serial and the repeat was never queued: once the show went, nothing else follows.
        let sent = await wire.sent(count: 2)
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

        let edges = await wire.sent(count: 2).compactMap { Self.visibility($0.text) }
        #expect(edges.map { $0.0 } == ["a", "b"])
        #expect(edges.map { $0.1 } == [false, false])
        socket.close(code: 1000, reason: "", error: nil)
    }

    @Test func aSocketNotOpenOrClosedSendsNothing() async throws {
        let wire = Wire()
        let socket = try Self.socket(wire)
        socket.visibilityChanged(session: "s", visible: false)
        try await socket.start(timeout: 1)
        socket.close(code: 1000, reason: "", error: nil)
        socket.visibilityChanged(session: "s", visible: true)
        #expect(wire.frames.isEmpty, "\(wire.frames.map(\.text))")
    }
}
