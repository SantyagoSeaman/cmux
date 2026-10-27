import CmuxNextCompat
import class CmuxSafeConcurrency.OnceContinuation
import enum CmuxSafeConcurrency.OnceContinuationError
import func CmuxSafeConcurrency.withOnceContinuation
import Foundation

/// One terminal view's bounded queue between its stream's reader
/// (``TerminalStreamReader``, on the connection's reader thread) and the view
/// that consumes `events`.
///
/// A push never waits: the reader thread serves every stream of its
/// connection and the control plane. A view that falls `highWater` output
/// bytes behind loses its own queued output (``PushOutcome/overflowed``) and
/// its stream resyncs by snapshot, so a slow view never stalls another one
/// (D5, app-daemon-connection-DESIGN 3.4): unlike a mojo endpoint's unbounded
/// queue (mojo/public/cpp/bindings/lib/multiplex_router.cc), a terminal can
/// always rebuild its screen from a snapshot. Each frame and ring span
/// stays its own event: chunks are never merged, so a ring span reaches
/// Ghostty uncopied and no bytes move past a grid change.
final class TerminalEventQueue: Sendable {
    /// What a push did.
    enum PushOutcome: Equatable {
        case queued
        /// Output passed `highWater`: every queued output chunk and history
        /// page was dropped (the new one too). The stream resyncs by snapshot.
        case overflowed
    }

    /// The output bytes a view may hold queued before its output is dropped.
    let highWater: Int
    private let state = Mutex(State())

    private struct State {
        var items: [TerminalChannelEvent] = []
        var head = 0
        var outputBytes = 0
        var waiter: OnceContinuation<TerminalChannelEvent?, OnceContinuationError>?
        var finished = false

        /// Drops every queued output chunk and history page; other events stay in order.
        mutating func dropQueuedOutput() {
            items = items.dropFirst(head).filter { TerminalEventQueue.outputSize($0) == 0 }
            head = 0
            outputBytes = 0
        }

        /// The next queued event; nil when none is queued.
        mutating func pop() -> TerminalChannelEvent? {
            guard let event = items[checked: head] else { return nil }
            head += 1
            outputBytes -= TerminalEventQueue.outputSize(event)
            if head > 256, head * 2 > items.count {
                items.removeFirst(head)
                head = 0
            }
            return event
        }
    }

    init(highWater: Int = 8 << 20) {
        self.highWater = highWater
    }

    /// Reader thread. Queues `event`, or hands it to a waiting consumer; never waits.
    @discardableResult
    func push(_ event: TerminalChannelEvent) -> PushOutcome {
        let (waiter, outcome) = state.withLock {
            state -> (OnceContinuation<TerminalChannelEvent?, OnceContinuationError>?, PushOutcome) in
            guard !state.finished else { return (nil, .queued) }
            if let waiter = state.waiter.take() { return (waiter, .queued) }
            state.items.append(event)
            state.outputBytes += Self.outputSize(event)
            guard state.outputBytes > highWater else { return (nil, .queued) }
            state.dropQueuedOutput()
            return (nil, .overflowed)
        }
        waiter?.resume(returning: event)
        return outcome
    }

    /// Any thread. Queues a small, non-output event that never counts toward `highWater`, so a
    /// caller may hold its own lock across it (a size state is pushed under the lock that orders
    /// generations) and an echo marker is never dropped with the output.
    func pushWithoutWaiting(_ event: TerminalChannelEvent) {
        let waiter = state.withLock { state -> OnceContinuation<TerminalChannelEvent?, OnceContinuationError>? in
            guard !state.finished else { return nil }
            if let waiter = state.waiter.take() { return waiter }
            state.items.append(event)
            state.outputBytes += Self.outputSize(event)
            return nil
        }
        waiter?.resume(returning: event)
    }

    /// Ends the stream after `last` (if any) and anything already queued.
    func finish(_ last: TerminalChannelEvent? = nil) {
        let waiter = state.withLock { state -> OnceContinuation<TerminalChannelEvent?, OnceContinuationError>? in
            guard !state.finished else { return nil }
            state.finished = true
            if let waiter = state.waiter.take() { return waiter }
            if let last { state.items.append(last) }
            return nil
        }
        waiter?.resume(returning: last)
    }

    /// Consumer drops the stream: discard everything.
    func cancel() {
        let waiter = state.withLock { state -> OnceContinuation<TerminalChannelEvent?, OnceContinuationError>? in
            state.finished = true
            state.items.removeAll()
            state.head = 0
            state.outputBytes = 0
            return state.waiter.take()
        }
        waiter?.resume(returning: nil)
    }

    /// Next event; nil once the stream ended, or when the parked waiter was dropped unresumed.
    func next() async -> TerminalChannelEvent? {
        try? await withOnceContinuation { (continuation: OnceContinuation<TerminalChannelEvent?, OnceContinuationError>) in
            takeOrWait(continuation)
        }
    }

    /// Resumes at once when an event is queued or the stream ended;
    /// otherwise parks the continuation for the next `push`/`finish`.
    private func takeOrWait(_ continuation: OnceContinuation<TerminalChannelEvent?, OnceContinuationError>) {
        let ready = state.withLock { state -> TerminalChannelEvent?? in
            if let event = state.pop() { return .some(event) }
            if state.finished { return .some(nil) }
            state.waiter = continuation
            return nil
        }
        if let ready { continuation.resume(returning: ready) }
    }

    var bufferedOutputBytes: Int {
        state.withLock { $0.outputBytes }
    }

    /// Bulk bytes: output and snapshot history. A READY is one screen and
    /// never counts (it is what a resync waits for).
    private static func outputSize(_ event: TerminalChannelEvent) -> Int {
        switch event {
        case .output(let data, _): data.count
        case .snapshot(let frame) where frame.phase != .ready: frame.data.count
        default: 0
        }
    }
}
