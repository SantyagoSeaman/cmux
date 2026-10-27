import CmuxNextCompat
import class CmuxSafeConcurrency.OnceContinuation
import enum CmuxSafeConcurrency.OnceContinuationError
import func CmuxSafeConcurrency.withOnceContinuation
import Foundation

/// Bounded, demand-driven buffer between an attach connection's reader
/// thread and the terminal view that consumes `events`.
///
/// Backpressure: once armed (after the attach response), the reader thread
/// blocks while more than `highWater` output bytes wait, and resumes below
/// `lowWater`. The socket then stops draining, so the daemon's bounded attach
/// mailbox overflows and it ends the stream with `overflow` instead of the
/// app buffering without limit; the view reattaches from a fresh replay.
/// Replays never block (a 10 MiB replay must not wedge the attach
/// handshake). Consecutive plain output chunks are merged on the way out, so a
/// consumer that fell behind takes up to `mergeLimit` bytes in one step:
/// decoded JSON chunks by a copy, attach-frame output (``TerminalOutputSpan``)
/// that continues in the PTY stream by range when the slices lie next to each
/// other in one read buffer, else by one copy. A consumer that keeps up finds
/// one span queued and gets it as a slice of its read buffer, never copied.
/// Blink coalesces queued input only while its main thread is behind the same
/// way (third_party/blink/renderer/platform/widget/input/main_thread_event_queue.cc).
/// Nothing merges across a grid replacement or any other event.
///
/// Ring output (``TerminalRingSpan``, p05-ipc-113) is queued as ranges of the
/// mapping, not bytes: spans of one mapping and screen that continue each other
/// in the PTY stream and in memory merge into one wider range, never across the
/// wrap, and each reaches the consumer as `Data` that points into the mapping.
/// Its bound is the ring itself, so it never parks the reader: a consumer more
/// than the ring behind finds its spans lapped. A span lapped before the
/// consumer takes it is dropped and marks its screen, and the stream resyncs
/// (app-daemon-connection-DESIGN.md sections 3.3 and 4.4).
final class TerminalEventQueue: Sendable {
    let highWater: Int
    let lowWater: Int
    let mergeLimit: Int
    /// Called on the reader thread each time a `push` starts to block above
    /// `highWater`, before it waits. Tests use it to observe backpressure
    /// without timing; it must not block.
    private let onReaderBlocked: (@Sendable () -> Void)?
    /// Counts the output bytes the queue copies when it merges chunks
    /// (``DaemonWireStats/Snapshot/outputBytesCopiedApp``); nil counts nothing.
    private let recorder: DaemonWireStats.Recorder?
    private let state = Mutex(State())
    // concurrency-allow: the designed backpressure that makes the daemon drop a slow view: only the attach
    // connection's dedicated reader thread (LineTransport) waits on it, never the main thread or a pool thread.
    private let readerWake = DispatchSemaphore(value: 0)

    /// One queued item: an event, live output still in its read buffer (attach frames), or live output still
    /// in the stream's output ring.
    private enum Item {
        case event(TerminalChannelEvent)
        case span(TerminalOutputSpan)
        case ring(TerminalRingSpan)

        /// The event the consumer gets; nil for a ring span the writer lapped before the consumer took it.
        var event: TerminalChannelEvent? {
            switch self {
            case .event(let event): event
            case .span(let span): .output(span.bytes.data, colors: nil)
            case .ring(let span): span.dataUnlessLapped().map { .output($0, colors: nil) }
            }
        }

        /// Bytes that count toward `highWater`; ring output holds none of the queue's memory.
        var outputSize: Int {
            switch self {
            case .event(let event): TerminalEventQueue.outputSize(event)
            case .span(let span): span.bytes.count
            case .ring: 0
            }
        }
    }

    private struct State {
        var items: [Item] = []
        var head = 0
        var outputBytes = 0
        var waiter: OnceContinuation<TerminalChannelEvent?, OnceContinuationError>?
        var finished = false
        var armed = false
        /// The reader waits on `readerWake`; whoever clears this signals it exactly once.
        var readerParked = false

        /// Whether the reader must wait now (armed, not finished, above `highWater`); marks it parked if so.
        mutating func parkReader(above highWater: Int) -> Bool {
            readerParked = armed && !finished && outputBytes > highWater
            return readerParked
        }

        /// Clears the parked mark; true when the caller must signal the reader.
        mutating func releaseReader() -> Bool {
            defer { readerParked = false }
            return readerParked
        }

        /// Next event, merging consecutive plain output chunks up to `mergeLimit`; nil when none is queued.
        /// Lapped ring spans are skipped.
        mutating func popMerged(mergeLimit: Int, recorder: DaemonWireStats.Recorder?) -> TerminalChannelEvent? {
            while let item = items[checked: head] {
                head += 1
                if let event = take(item, mergeLimit: mergeLimit, recorder: recorder) {
                    compact()
                    return event
                }
            }
            compact()
            return nil
        }

        /// The event `item` (just popped) gives, merged with the items after it; nil for a lapped ring span.
        private mutating func take(_ item: Item, mergeLimit: Int,
                                   recorder: DaemonWireStats.Recorder?) -> TerminalChannelEvent? {
            let event: TerminalChannelEvent
            switch item {
            case .ring(let first):
                var joined = first  // one range of one mapping: merging copies nothing
                while joined.count < mergeLimit, case .ring(let next)? = items[checked: head],
                      let wider = joined.joined(with: next) {
                    joined = wider
                    head += 1
                }
                return Item.ring(joined).event
            case .span(let first):
                var joined: TerminalOutputSpan? = first  // the merged bytes while they are one slice of one buffer
                var copied = Data()  // the merged bytes once they are not
                var end = first.end
                var count = first.bytes.count
                while count < mergeLimit, case .span(let next)? = items[checked: head], next.offset == end {
                    if let wider = joined?.joined(with: next) {
                        joined = wider
                    } else {
                        if let run = joined.take() { copied.append(run.bytes.data) }
                        copied.append(next.bytes.data)
                    }
                    end = next.end
                    count += next.bytes.count
                    head += 1
                }
                if let joined {
                    event = .output(joined.bytes.data, colors: nil)  // one wider slice, no copy
                } else {
                    recorder?.outputCopied(copied.count)  // a merge that writes every byte of the result
                    event = .output(copied, colors: nil)
                }
                outputBytes -= count
            case .event(.output(var data, nil)):
                var merged = false
                while data.count < mergeLimit, case .event(.output(let more, nil))? = items[checked: head] {
                    data.append(more)
                    head += 1
                    merged = true
                }
                // A merge writes every byte of the result into a new buffer.
                if merged { recorder?.outputCopied(data.count) }
                event = .output(data, colors: nil)
                outputBytes -= data.count
            case .event(let other):
                event = other
                outputBytes -= TerminalEventQueue.outputSize(other)
            }
            return event
        }

        private mutating func compact() {
            guard head > 256, head * 2 > items.count else { return }
            items.removeFirst(head)
            head = 0
        }
    }

    init(highWater: Int = 8 << 20, lowWater: Int = 2 << 20, mergeLimit: Int = 1 << 20,
         recorder: DaemonWireStats.Recorder?, onReaderBlocked: (@Sendable () -> Void)? = nil) {
        self.highWater = highWater
        self.lowWater = lowWater
        self.mergeLimit = mergeLimit
        self.recorder = recorder
        self.onReaderBlocked = onReaderBlocked
    }

    /// Enables blocking once the consumer can start draining.
    func arm() {
        state.withLock { $0.armed = true }
    }

    /// Reader thread. May block while the consumer is behind.
    func push(_ event: TerminalChannelEvent) {
        enqueue(.event(event))
    }

    /// Reader thread. Live output of an attach frame, still in its read buffer; it reaches the consumer as a
    /// slice of that buffer. May block while the consumer is behind, as pushing an event does.
    func push(_ span: TerminalOutputSpan) {
        enqueue(.span(span))
    }

    /// Reader thread. Live output still in the stream's output ring; it reaches the consumer as `Data` that
    /// points into the mapping. Never blocks: the ring bounds it.
    func push(_ span: TerminalRingSpan) {
        enqueue(.ring(span))
    }

    private func enqueue(_ item: Item) {
        typealias Handoff = (waiter: OnceContinuation<TerminalChannelEvent?, OnceContinuationError>, event: TerminalChannelEvent)
        let (handoff, blocked) = state.withLock { state -> (Handoff?, Bool) in
            guard !state.finished else { return (nil, false) }
            if state.waiter != nil {
                // The consumer waits, so nothing is queued: it takes this item now (a lapped ring span is dropped).
                guard let event = item.event, let waiter = state.waiter.take() else { return (nil, false) }
                return ((waiter, event), false)
            }
            state.items.append(item)
            state.outputBytes += item.outputSize
            return (nil, state.parkReader(above: highWater))
        }
        handoff?.waiter.resume(returning: handoff?.event)
        guard blocked else { return }
        onReaderBlocked?()
        var parked = blocked
        while parked {
            // The designed backpressure that makes the daemon drop a slow view: a checked wait, only on the attach
            // connection's dedicated reader thread (LineTransport), never the main thread or a pool thread.
            readerWake.blockingWait()
            parked = state.withLock { $0.parkReader(above: highWater) }
        }
    }

    /// Any thread. Queues a small, non-output event without the backpressure wait, so a caller may
    /// hold its own lock across it (a size state is pushed under the lock that orders generations).
    func pushWithoutWaiting(_ event: TerminalChannelEvent) {
        let waiter = state.withLock { state -> OnceContinuation<TerminalChannelEvent?, OnceContinuationError>? in
            guard !state.finished else { return nil }
            if let waiter = state.waiter.take() { return waiter }
            state.items.append(.event(event))
            state.outputBytes += Self.outputSize(event)
            return nil
        }
        waiter?.resume(returning: event)
    }

    /// Ends the stream after `last` (if any) and anything already queued.
    /// Unblocks a waiting reader.
    func finish(_ last: TerminalChannelEvent? = nil) {
        let (waiter, release) = state.withLock { state -> (OnceContinuation<TerminalChannelEvent?, OnceContinuationError>?, Bool) in
            guard !state.finished else { return (nil, false) }
            state.finished = true
            let release = state.releaseReader()
            if let waiter = state.waiter.take() { return (waiter, release) }
            if let last { state.items.append(.event(last)) }
            return (nil, release)
        }
        if release { readerWake.signal() }
        waiter?.resume(returning: last)
    }

    /// Consumer drops the stream: discard everything and release the reader.
    func cancel() {
        let (waiter, release) = state.withLock { state -> (OnceContinuation<TerminalChannelEvent?, OnceContinuationError>?, Bool) in
            state.finished = true
            state.items.removeAll()
            state.head = 0
            state.outputBytes = 0
            return (state.waiter.take(), state.releaseReader())
        }
        if release { readerWake.signal() }
        waiter?.resume(returning: nil)
    }

    /// Next event, merging consecutive plain output chunks up to `mergeLimit`; nil once the stream ended,
    /// or when the parked waiter was dropped unresumed.
    func next() async -> TerminalChannelEvent? {
        try? await withOnceContinuation { (continuation: OnceContinuation<TerminalChannelEvent?, OnceContinuationError>) in
            takeOrWait(continuation)
        }
    }

    /// Resumes at once when an event is queued or the stream ended;
    /// otherwise parks the continuation for the next `push`/`finish`.
    private func takeOrWait(_ continuation: OnceContinuation<TerminalChannelEvent?, OnceContinuationError>) {
        let (ready, release) = state.withLock { state -> (TerminalChannelEvent??, Bool) in
            if let event = state.popMerged(mergeLimit: mergeLimit, recorder: recorder) {
                // At or below `lowWater` the reader reads again.
                guard state.outputBytes <= lowWater else { return (.some(event), false) }
                return (.some(event), state.releaseReader())
            }
            if state.finished { return (.some(nil), false) }
            state.waiter = continuation
            return (nil, false)
        }
        if release { readerWake.signal() }
        if let ready { continuation.resume(returning: ready) }
    }

    var bufferedOutputBytes: Int {
        state.withLock { $0.outputBytes }
    }

    /// Bulk bytes: output and snapshot history. A READY, like a replay,
    /// never blocks the reader (it is one screen, sent before the attach
    /// reply on attach).
    private static func outputSize(_ event: TerminalChannelEvent) -> Int {
        switch event {
        case .output(let data, _): data.count
        case .snapshot(let frame) where frame.phase != .ready: frame.data.count
        default: 0
        }
    }
}
