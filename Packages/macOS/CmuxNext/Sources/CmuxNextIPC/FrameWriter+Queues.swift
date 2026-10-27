import Foundation

// The priority FIFOs of ``FrameWriter``: what waits, what one drain step takes, coalescing, and the continuous
// messages held for the next display refresh (p05-ipc-055, p05-ipc-141).

extension FrameWriter {
    struct Message {
        var bytes: Data
        let fds: [Int32]
        /// Set on a continuous message: a newer one with this key replaces it while it is unsent.
        let key: CoalesceKey?
    }

    /// One drain step: messages in wire order; `fds` go with the first byte.
    struct Batch {
        var messages: [Data]
        let fds: [Int32]
        var byteCount: Int

        func joined() -> Data {
            var data = Data(capacity: byteCount)
            for message in messages { data.append(message) }
            return data
        }
    }

    /// One priority's FIFO: popped slots are cleared at once, so a sent message's bytes are not retained. Messages
    /// from `released` on are continuous messages held for the next display refresh; everything before may go.
    struct Lane {
        private var slots: [Message?] = []
        private var head = 0
        private var released = 0

        var first: Message? { head < released ? slots[head] : nil }

        mutating func append(_ message: Message, held: Bool) {
            slots.append(message)
            // A message that is not held releases the held ones before it: they keep their place in the FIFO
            // (Blink's MainThreadEventQueue dispatches every event up to the last one that is not rAF-aligned).
            if !held { released = slots.count }
        }

        /// The display refresh came: every held message may go.
        mutating func releaseHeld() {
            released = slots.count
        }

        /// The unsent message with `key` among the keyed messages at the FIFO's tail: the walk back passes
        /// messages of other keys and stops at one without a key (cmux-ipc's LaneWriter, p05-ipc-141).
        func coalescingIndex(_ key: CoalesceKey) -> Int? {
            var index = slots.count - 1
            // wakeup-allow: bounded: one pass over the unsent tail of this FIFO
            while index >= head, let queued = slots[index]?.key {
                if queued == key { return index }
                index -= 1
            }
            return nil
        }

        func byteCount(at index: Int) -> Int { slots[index]?.bytes.count ?? 0 }

        mutating func replaceBytes(at index: Int, with bytes: Data) {
            slots[index]?.bytes = bytes
        }

        mutating func popFirst() -> Message? {
            guard head < released else { return nil }
            let message = slots[head]
            slots[head] = nil
            head += 1
            if head == slots.count {
                slots.removeAll(keepingCapacity: true)
                head = 0
                released = 0
            } else if head >= 64, head * 2 >= slots.count {
                slots.removeFirst(head)
                released -= head
                head = 0
            }
            return message
        }

        /// Empties the FIFO; returns the descriptors of the dropped messages.
        mutating func removeAll() -> [Int32] {
            let fds = slots[head...].flatMap { $0?.fds ?? [] }
            slots.removeAll()
            head = 0
            released = 0
            return fds
        }
    }

    struct State {
        /// Indexed by ``MethodPriority/rawValue``: input, control, bulk, background.
        private var lanes = [Lane](repeating: Lane(), count: MethodPriority.allCases.count)
        private(set) var queuedBytes = 0
        var draining = false
        var closed = false
        var failure: Failure?
        /// A refresh was asked for the held messages and has not come yet.
        var refreshRequested = false

        /// Whether a message may be sent now.
        var hasReady: Bool { lanes.contains { $0.first != nil } }

        mutating func append(_ message: Message, priority: MethodPriority, held: Bool) {
            lanes[Int(priority.rawValue)].append(message, held: held)
            queuedBytes += message.bytes.count
        }

        /// Puts `bytes` in place of the unsent message of `priority` with `key`. nil when there is none to
        /// replace; else whether the replacement fit in `limit` (it is made only then).
        mutating func replace(_ bytes: Data, key: CoalesceKey, priority: MethodPriority, limit: Int) -> Bool? {
            let lane = Int(priority.rawValue)
            guard let index = lanes[lane].coalescingIndex(key) else { return nil }
            let queued = queuedBytes - lanes[lane].byteCount(at: index) + bytes.count
            guard queued <= limit else { return false }
            lanes[lane].replaceBytes(at: index, with: bytes)
            queuedBytes = queued
            return true
        }

        mutating func releaseHeld() {
            for index in lanes.indices { lanes[index].releaseHeld() }
        }

        /// The next drain step: the head of the highest non-empty FIFO. When that head is input or control, the
        /// following input and control heads join it in priority order while they fit in `budget`, stopping at a
        /// message with descriptors (its descriptors must ride its own first byte). A bulk or background head is
        /// sent alone: one chunk is the preemption boundary (p05-ipc.md section 3.2). nil, and draining ends,
        /// when there is nothing to send.
        mutating func takeBatch(budget: Int) -> Batch? {
            guard !closed, failure == nil, let first = popHighest() else {
                draining = false
                return nil
            }
            var batch = Batch(messages: [first.message.bytes], fds: first.message.fds,
                              byteCount: first.message.bytes.count)
            guard Self.joins(first.priority) else {
                queuedBytes -= batch.byteCount
                return batch
            }
            while let head = peekHighest(), Self.joins(head.priority), head.message.fds.isEmpty,
                  batch.byteCount + head.message.bytes.count <= budget {
                let next = head.message
                _ = popHighest()
                batch.messages.append(next.bytes)
                batch.byteCount += next.bytes.count
            }
            queuedBytes -= batch.byteCount
            return batch
        }

        mutating func removeAll() -> [Int32] {
            queuedBytes = 0
            var fds: [Int32] = []
            for index in lanes.indices { fds += lanes[index].removeAll() }
            return fds
        }

        /// Whether a message of `priority` may share a send with other waiting messages: only input and control,
        /// which are small; bulk and background are cut at BULK_FRAME_MAX and each chunk is its own step.
        private static func joins(_ priority: MethodPriority) -> Bool {
            switch priority {
            case .input, .control: true
            case .bulk, .background: false
            }
        }

        private func peekHighest() -> (message: Message, priority: MethodPriority)? {
            for priority in MethodPriority.allCases {
                if let message = lanes[Int(priority.rawValue)].first { return (message, priority) }
            }
            return nil
        }

        private mutating func popHighest() -> (message: Message, priority: MethodPriority)? {
            for priority in MethodPriority.allCases {
                if let message = lanes[Int(priority.rawValue)].popFirst() { return (message, priority) }
            }
            return nil
        }
    }
}
