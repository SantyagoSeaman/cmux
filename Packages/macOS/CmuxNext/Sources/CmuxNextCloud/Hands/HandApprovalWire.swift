public import Foundation
import CmuxNextWakeups

/// The owner's user socket (`/v1/wire/user`, `cmux.wire/1`) read for the hand frames only
/// (docs/chief-hands.md): `hand-approval-pending {hand, id, summary, at, sign}` and
/// `hand-approval-settled {hand, id, outcome}`. The frames are ephemeral (no replay), so each
/// subscribe answer (`connected`) tells the service to read the waiting list again. The bearer goes
/// in the Authorization header; reconnects back off.
///
/// The socket is silent while idle (p14-energy-121): the owner never pings it, and this side sends an
/// RFC 6455 ping only after ``keepaliveSilence`` with no frame either way (``SilenceKeepalive``),
/// which Cloudflare answers without waking the owner's Durable Object. No pong within ``pongLimit``
/// closes the socket, and the run loop reconnects.
@MainActor
final class HandApprovalWire {
    enum Event: Sendable {
        /// Subscribed (also after a reconnect): read the waiting approvals now.
        case connected
        case pending(HandApproval)
        case settled(hand: String, id: String, outcome: HandApprovalOutcome)
    }

    private let url: URL
    private let token: @Sendable () async throws -> String
    private var runner: Task<Void, Never>?
    private var generation = 0
    /// The silence, both ways, before the keepalive pings: short enough that no proxy on the path
    /// reaps the idle socket, long enough that an idle Mac sends one ping a minute.
    nonisolated static let keepaliveSilence: Duration = .seconds(60)
    /// How long a keepalive ping waits for its pong before the socket is closed.
    nonisolated static let pongLimit: Duration = .seconds(10)
    /// Pings the open socket after ``keepaliveSilence`` with no frame either way.
    let keepalive: SilenceKeepalive
    /// The pong deadline of the latest keepalive ping.
    private let pong: DemandTimer

    init(apiBaseURL: URL, clock: any Clock<Duration> = ContinuousClock(),
         keepaliveSilence: Duration = HandApprovalWire.keepaliveSilence,
         token: @escaping @Sendable () async throws -> String) {
        var c = URLComponents(url: apiBaseURL, resolvingAgainstBaseURL: false) ?? URLComponents()
        c.scheme = c.scheme == "http" ? "ws" : "wss"
        c.path = "/v1/wire/user"
        url = c.url ?? apiBaseURL
        self.token = token
        keepalive = SilenceKeepalive(owner: "cloud.wire.keepalive", silence: keepaliveSilence, clock: clock)
        // timer-class-allow: off-main: the pong deadline only closes the pinged socket (thread-safe) and runs no main-thread job.
        pong = DemandTimer(owner: "HandApprovalWire.pong", clock: clock)
    }

    func start(_ sink: @escaping @MainActor (Event) -> Void) {
        stop()
        let generation = generation
        runner = Task { [weak self] in await self?.run(generation, sink: sink) }
    }

    func stop() {
        generation += 1
        runner?.cancel()
        runner = nil
        keepalive.stop()
        pong.cancel()
    }

    /// One keepalive ping on `socket`: its pong re-arms the keepalive and drops the deadline; a failed
    /// ping or no pong within ``pongLimit`` closes the socket.
    func ping(_ socket: any HandKeepaliveSocket) {
        let (pong, keepalive) = (pong, keepalive)
        pong.schedule(after: Self.pongLimit) { socket.keepaliveClose() }
        socket.keepalivePing { failed in
            pong.cancel()
            if failed { socket.keepaliveClose() } else { keepalive.heard() }
        }
    }

    /// A frame went in or out of the open socket: it is alive, and the keepalive waits a full silence.
    func heard() {
        keepalive.heard()
        pong.cancel()
    }

    private func run(_ generation: Int, sink: @escaping @MainActor (Event) -> Void) async {
        var backoff = Backoff(initial: .milliseconds(500), maximum: .seconds(30))
        // wakeup-allow: backoff: one pass per connection; each pass awaits the socket and, after a failure, a Backoff delay.
        while !Task.isCancelled, generation == self.generation {
            do {
                try await session(generation, sink: sink, onSubscribed: { backoff.reset() })
            } catch is CancellationError {
                return
            } catch {}
            // concurrency-allow: Backoff.wait is an async sleep after a failure, not a blocking wait.
            do { try await backoff.wait(owner: "hands.wire.reconnect") } catch { return }
        }
    }

    private func session(_ generation: Int, sink: @MainActor (Event) -> Void, onSubscribed: () -> Void) async throws {
        var request = URLRequest(url: url)
        request.setValue("cmux.wire.v1", forHTTPHeaderField: "Sec-WebSocket-Protocol")
        request.setValue("Bearer \(try await token())", forHTTPHeaderField: "Authorization")
        // updates.minimumVersion: an owner refuses a client that does not name its version.
        if let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String {
            request.setValue(version, forHTTPHeaderField: "x-cmux-client-version")
        }
        let task = URLSession.shared.webSocketTask(with: request)
        // The subscribe answer is the user owner's snapshot (up to about 1.5 MB).
        task.maximumMessageSize = 8 << 20
        task.resume()
        keepalive.start { [weak self] in self?.ping(task) }
        defer {
            // A stopped or restarted wire already dropped them; a newer session's are not this one's.
            if generation == self.generation {
                keepalive.stop()
                pong.cancel()
            }
            task.cancel(with: .goingAway, reason: nil)
        }
        // receive() ignores task cancellation: stop() closes the socket, so a stopped run posts nothing more.
        try await withTaskCancellationHandler {
            // wakeup-allow: bounded: each turn awaits the next frame; a closed socket throws and ends the loop.
            while !Task.isCancelled {
                let message = try await task.receive()
                heard()
                let data: Data
                switch message {
                case let .string(text): data = Data(text.utf8)
                case let .data(bytes): data = bytes
                @unknown default: continue
                }
                let frame = await Task.detached(priority: Task.currentPriority) { Frame.decode(data) }.value
                guard generation == self.generation else { throw CancellationError() }
                switch frame {
                case .welcome:
                    try await task.send(.string(#"{"t":"subscribe"}"#))
                    heard()
                case .snapshot:
                    onSubscribed()
                    sink(.connected)
                case let .event(event):
                    sink(event)
                case .ignored:
                    break
                }
            }
            throw CancellationError()
        } onCancel: {
            task.cancel(with: .goingAway, reason: nil)
        }
    }

    /// One frame, decoded off the main actor (a snapshot is large; only its type is read).
    nonisolated enum Frame: Sendable {
        case welcome, snapshot, ignored
        case event(Event)

        static func decode(_ data: Data) -> Frame {
            guard let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { return .ignored }
            switch object["t"] as? String {
            case "welcome": return .welcome
            case "snapshot": return .snapshot
            case "hand-approval-pending":
                return HandApproval(object).map { .event(.pending($0)) } ?? .ignored
            case "hand-approval-settled":
                guard let hand = object["hand"] as? String, HandIDs.isHand(hand), let id = object["id"] as? String,
                      HandIDs.isApproval(id), let outcome = (object["outcome"] as? String).flatMap(HandApprovalOutcome.init(rawValue:))
                else { return .ignored }
                return .event(.settled(hand: hand, id: id, outcome: outcome))
            default: return .ignored
            }
        }
    }
}

/// The socket a keepalive ping goes out on: the session's `URLSessionWebSocketTask`; tests pass a fake.
protocol HandKeepaliveSocket: Sendable {
    /// Sends an RFC 6455 ping; `answered(failed)` runs once, when the pong comes or the send fails.
    func keepalivePing(_ answered: @escaping @Sendable (_ failed: Bool) -> Void)
    func keepaliveClose()
}

extension URLSessionWebSocketTask: HandKeepaliveSocket {
    func keepalivePing(_ answered: @escaping @Sendable (_ failed: Bool) -> Void) {
        sendPing(pongReceiveHandler: { error in answered(error != nil) })
    }

    func keepaliveClose() {
        cancel(with: .goingAway, reason: nil)
    }
}
