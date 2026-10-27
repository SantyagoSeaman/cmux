import Foundation

// The priority FIFOs of ``FrameWriter``: what waits, what one drain step takes, and coalescing (p05-ipc-055,
// p05-ipc-141).

extension FrameWriter {
    struct Message {
        var bytes: Data
        let fds: [Int32]
        /// Set on a continuous message: a newer one with this key merges into it while it is unsent.
        let key: CoalesceKey?
        /// The caller's name for a continuous message (its request id), reported when a newer one replaces it.
        var identity: UInt64
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

    /// One priority's FIFO: popped slots are cleared at once, so a sent message's bytes are not retained.
    struct Lane {
        private var slots: [Message?] = []
        private var head = 0

        var first: Message? { head < slots.count ? slots[head] : nil }

        mutating func append(_ message: Message) {
            slots.append(message)
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

        func message(at index: Int) -> Message? { slots[index] }

        /// Puts `bytes` and `identity` in place of the message at `index`; its key, place and descriptors stay.
        mutating func replace(at index: Int, bytes: Data, identity: UInt64) {
            slots[index]?.bytes = bytes
            slots[index]?.identity = identity
        }

        mutating func popFirst() -> Message? {
            guard head < slots.count else { return nil }
            let message = slots[head]
            slots[head] = nil
            head += 1
            if head == slots.count {
                slots.removeAll(keepingCapacity: true)
                head = 0
            } else if head >= 64, head * 2 >= slots.count {
                slots.removeFirst(head)
                head = 0
            }
            return message
        }

        /// Empties the FIFO; returns the descriptors of the dropped messages.
        mutating func removeAll() -> [Int32] {
            let fds = slots[head...].flatMap { $0?.fds ?? [] }
            slots.removeAll()
            head = 0
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

        mutating func append(_ message: Message, priority: MethodPriority) {
            lanes[Int(priority.rawValue)].append(message)
            queuedBytes += message.bytes.count
        }

        /// Merges `message` into the unsent message of `priority` with its key: `merge` of the two, or `message`
        /// when `merge` is nil, takes the queued one's place. nil when `message` has no key or none is queued to
        /// merge into; else the replaced identity, or why the merge did not fit in `limit` (nothing changes then).
        mutating func merge(_ message: Message, priority: MethodPriority, merge: Continuous.Merge?,
                            limit: Int) -> Coalesced? {
            let lane = Int(priority.rawValue)
            guard let key = message.key, let index = lanes[lane].coalescingIndex(key),
                  let queued = lanes[lane].message(at: index) else { return nil }
            let bytes = merge.map { $0(queued.bytes, message.bytes) } ?? message.bytes
            let total = queuedBytes - queued.bytes.count + bytes.count
            guard total <= limit else { return .refused(.backlog(queued: queuedBytes)) }
            lanes[lane].replace(at: index, bytes: bytes, identity: message.identity)
            queuedBytes = total
            return .replaced(identity: queued.identity)
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
