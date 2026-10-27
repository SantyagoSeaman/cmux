import Foundation

/// Ring output bookkeeping of ``TerminalEventQueue`` (p05-ipc-113), under the queue's lock: queued runs, and the
/// lap claims that drop them. A claim's repair runs after the lock (``takeRepairs()``).
extension TerminalEventQueue.State {
    /// Repairs owed by claims made under the lock, now the caller's to run after it.
    mutating func takeRepairs() -> [TerminalRingLaps] {
        defer { repairs.removeAll() }
        return repairs
    }

    /// Claims `laps` lapped and drops its queued runs; the first claim owes the repair.
    mutating func lapped(_ laps: TerminalRingLaps) {
        if laps.claim() { repairs.append(laps) }
        items.removeSubrange(..<head)
        head = 0
        items.removeAll { item in
            guard case .ring(let run) = item else { return false }
            return run.laps === laps
        }
    }

    /// Queues ring output, or hands it to a waiting consumer as it was cut. A span of a screen claimed lapped
    /// is dropped. Before it is queued, the oldest queued run of its ring is checked: once the writer laps it,
    /// its screen is claimed and its runs go, so ring runs never hold more than one ring of bytes, and the
    /// stream asks for the fresh screen now rather than when the consumer comes back.
    mutating func enqueue(ring span: TerminalRingSpan,
                          mergeLimit: Int) -> (TerminalEventQueue.Waiter, TerminalChannelEvent)? {
        let run = span.run
        while !finished, let oldest = oldestRun(of: run.ring), oldest.isLapped {
            lapped(oldest.laps)
        }
        guard !finished, !run.laps.isMarked else { return nil }
        if let waiter = waiter.take() { return (waiter, .output(span.bytes, colors: nil)) }
        if items.count > head, case .ring(let tail)? = items.last, tail.count < UInt64(mergeLimit),
           let wider = tail.joined(with: run) {
            items[items.count - 1] = .ring(wider)  // one range of one mapping: O(1) items however long the stall
        } else {
            items.append(.ring(run))
        }
        return nil
    }

    /// `first` (just popped) merged with the runs after it that continue it, cut from the ring now; nil when its
    /// screen was claimed lapped or the ring refuses it, which claims the screen.
    mutating func take(ring first: TerminalRingRun, mergeLimit: Int) -> TerminalChannelEvent? {
        guard !first.laps.isMarked else { return nil }
        var joined = first  // one range of one mapping: merging copies nothing
        while joined.count < UInt64(mergeLimit), case .ring(let next)? = items[checked: head],
              let wider = joined.joined(with: next) {
            joined = wider
            head += 1
        }
        // A run never crosses the wrap, so the ring cuts it as one span.
        guard let cut = try? joined.cut(), cut.tail == nil else {
            lapped(joined.laps)
            return nil
        }
        return .output(cut.head.bytes, colors: nil)
    }

    /// The first queued run of `ring`: it starts before every later one.
    private func oldestRun(of ring: any TerminalOutputRing) -> TerminalRingRun? {
        for item in items[head...] {
            if case .ring(let run) = item, run.ring === ring { return run }
        }
        return nil
    }
}
