import CmuxNextCompat
import Foundation

/// The bounded queue between a connection's reader thread and one agent attachment's page: the reader only
/// queues, and the page's handler runs off the reader, in order, so a slow page never delays the connection's
/// other traffic. Past ``Limits`` it takes nothing more and ends with `overflow`, as the daemon's own stream does
/// (the page attaches again after the newest record it got, so what it got stays a gap-free prefix). Chrome
/// queues each mojo endpoint's messages and runs them on the endpoint's sequence, never on the IO thread
/// (mojo/public/cpp/bindings/lib/multiplex_router.cc).
final class AgentSessionEventQueue: Sendable {
    /// How much may wait for the page.
    struct Limits: Sendable, Equatable {
        /// Events: the daemon's per-connection mailbox.
        var events = 4096
        /// Bytes: two of the largest lines the daemon sends (an acpmux line is at most 32 MiB).
        var bytes = 64 << 20
    }

    /// What `push` did with an event.
    enum Admission: Equatable {
        case queued
        /// It did not fit: the queue takes nothing more and ends with `overflow` after what it holds.
        case overflowed
        /// The queue already ended.
        case ended
    }

    private enum Item: Sendable {
        case event(name: String, line: Data)
        /// The queue's last delivery: an `agent-session-closed` line.
        case end(Data)
    }

    private struct Budget {
        var events = 0
        var bytes = 0
        /// Takes no more events.
        var ended = false
        /// Delivers nothing more.
        var cancelled = false
    }

    private let limits: Limits
    private let budget = Mutex(Budget())
    private let continuation: AsyncStream<Item>.Continuation

    /// - Parameters:
    ///   - limits: How much may wait for `deliver`.
    ///   - deliver: Takes each event in order, off the reader thread.
    init(limits: Limits = Limits(), deliver: @escaping @Sendable (_ event: String, _ line: Data) -> Void) {
        self.limits = limits
        // The budget admits at most `limits.events` events, and the end follows them: the buffer never drops one.
        let (items, continuation) = AsyncStream<Item>.makeStream(bufferingPolicy: .bufferingOldest(limits.events + 1))
        self.continuation = continuation
        // task-owner: drains this attachment's events in order; ends with the queue (end or cancel)
        Task { [self] in await drain(items, deliver) }
    }

    /// Queues one event, on the reader thread.
    func push(_ name: String, _ line: Data) -> Admission {
        let admission = budget.withLock { budget -> Admission in
            guard !budget.ended else { return .ended }
            guard budget.events < limits.events, budget.bytes + line.count <= limits.bytes else {
                budget.ended = true
                return .overflowed
            }
            budget.events += 1
            budget.bytes += line.count
            return .queued
        }
        if admission == .queued { continuation.yield(.event(name: name, line: line)) }
        return admission
    }

    /// Takes no more events; `closing` (an `agent-session-closed` line) is delivered after the queued ones,
    /// nil delivers none.
    func end(closing: Data?) {
        budget.withLock { $0.ended = true }
        if let closing { continuation.yield(.end(closing)) }
        continuation.finish()
    }

    /// Delivers nothing more, not even what is queued.
    func cancel() {
        budget.withLock {
            $0.ended = true
            $0.cancelled = true
        }
        continuation.finish()
    }

    /// Events and bytes waiting for the page (tests).
    var queued: (events: Int, bytes: Int) { budget.withLock { ($0.events, $0.bytes) } }

    private func drain(_ items: AsyncStream<Item>, _ deliver: @Sendable (String, Data) -> Void) async {
        for await item in items {
            switch item {
            case .event(let name, let line):
                let live = budget.withLock { budget -> Bool in
                    budget.events -= 1
                    budget.bytes -= line.count
                    return !budget.cancelled
                }
                if live { deliver(name, line) }
            case .end(let line):
                if !budget.withLock({ $0.cancelled }) { deliver(AgentSessionStream.closedEvent, line) }
                return
            }
        }
    }
}
