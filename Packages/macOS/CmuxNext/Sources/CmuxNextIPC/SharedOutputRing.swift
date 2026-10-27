import CmuxNextCompat
import Darwin
public import Foundation

/// The app's read-only view of a terminal's output ring, `CMAR` layout 1 (plans/cmux-next/app-daemon-connection-DESIGN.md,
/// section 4; plans/cmux-next/chrome-parity/p05-ipc.md, task 112). The daemon writes the ring; the app maps it
/// `PROT_READ` and hands Ghostty its bytes in place, as `Data` that points into the mapping.
///
/// The ring is reference counted: the stream that received it holds one reference and every span it cut holds
/// one (the span's `Data` deallocator). The last release unmaps, so detach, stream end, connection loss and quit
/// unmap only after Ghostty dropped the last span, never under it.
///
/// Reading follows the one-writer seqlock of Chromium's `device/base/synchronization/one_writer_seqlock.h` with
/// the stream offsets as the sequence, as the Rust reader does (cmux-tui/crates/cmux-ipc/src/output_ring.rs,
/// `OutputRingReader`): the writer stores `reserve`, copies, then publishes `end`, and never waits. A reader of
/// `[s, t)` may use the bytes only while `reserve <= s + capacity`. ``cut(from:to:)`` checks that before any
/// span exists; each span checks it again when its `Data` is released (after the parse), and a lap found then
/// marks the ring lapped (``takeLapped()``): the stream's next event is the resync.
///
/// Spans are read-only: a mutation of one would write a `PROT_READ` page. A span of 14 bytes or fewer is copied
/// by `Data` itself (its inline representation) and released at once, so it gets its release check right
/// after that copy.
///
/// Consumer cursor (RING_ACK, p05-ipc-119). The ring knows which of its bytes the app may still read in place:
/// every live span holds a slot with its start until its release (after Ghostty parsed it). ``takeAck()``, once
/// per display frame, gives the offset to send in a RING_ACK: the start of the oldest live span, or, with none
/// live, the end of the last cut (at least the ring's base). The daemon never writes past that offset plus the
/// capacity; it skips output instead, moves the ring's base past it, and holds the stream's READY until an ack
/// reaches the new base, which this ring sends once the app holds no span of the old screen. A held span is
/// then never overwritten, so the release check above only counts (``releaseLapCount`` stays 0 with a daemon that
/// honors the cursor). Chromium's data pipes give their producer the same bound, the consumer's read position
/// (mojo/core/data_pipe_producer_dispatcher.cc). The parser must read a span's own `Data` (or keep it alive
/// while it reads the bytes): a span released early frees its slot, and the next ack may let the writer reuse
/// its bytes.
public final class SharedOutputRing: Sendable {
    /// Bytes before the data area.
    public static let headerByteCount = 64
    /// The layout this build reads.
    public static let layout: UInt32 = 1
    /// The largest capacity a header may claim: a damaged header must not make the app map gigabytes.
    public static let maxCapacity: UInt64 = 64 << 20
    /// Live spans the ring tracks by start. A stream holds a few (the one Ghostty parses and the lane's next ones);
    /// past this many, ``takeAck()`` sends nothing until the extra spans are released.
    public static let heldSlotCount = 32
    private static let freeSlot = UInt64.max

    private static let magic: [UInt8] = Array("CMAR".utf8)
    private static let layoutOffset = 4
    private static let capacityOffset = 8
    private static let baseOffset = 16
    private static let endOffset = 24
    private static let reserveOffset = 32

    /// Why a ring descriptor was refused. Each is a protocol error that ends the connection (DESIGN 4.3).
    public enum OpenError: Error, Sendable, Equatable {
        /// The descriptor maps `PROT_WRITE, MAP_SHARED`: the app could change the ring for every reader.
        case writable
        /// `fstat` of the descriptor failed.
        case statFailed(errno: Int32)
        /// The read-only mapping failed.
        case mapFailed(errno: Int32)
        /// The object is smaller than its header and capacity need.
        case tooSmall(size: UInt64, needed: UInt64)
        /// The first four bytes are not `CMAR`.
        case badMagic
        /// A layout this build does not read.
        case badLayout(UInt32)
        /// A capacity of 0 or above ``SharedOutputRing/maxCapacity``.
        case badCapacity(UInt64)
        /// The header's end is before its base.
        case endBeforeBase(base: UInt64, end: UInt64)
    }

    /// Why `[from, to)` may not be cut: the Rust `SpanError`.
    public enum SpanError: Error, Sendable, Equatable {
        /// The writer has overwritten, or may be overwriting, part of the range: it is longer than the capacity,
        /// or the writer reserved past `from + capacity`. The stream resyncs.
        case lapped(from: UInt64, reserve: UInt64)
        /// The range ends past the last published byte.
        case notPublished(to: UInt64, end: UInt64)
        /// The range starts before the first byte this ring held.
        case beforeBase(from: UInt64, base: UInt64)
        /// The range ends before it starts.
        case backwards(from: UInt64, to: UInt64)
    }

    /// Where a live span is tracked until its release.
    private enum HeldSlot {
        case slot(AtomicPointerSlot)
        /// Every slot was taken: counted in `untracked`, which holds back the ack.
        case untracked
    }

    /// The bytes of one cut range, in stream order: one span, or two when the range crosses the wrap. Both are
    /// nil for an empty range.
    public struct Spans: Sendable {
        /// The stream offsets the spans cover.
        public let range: Range<UInt64>
        /// The bytes from `range.lowerBound`.
        public let head: Data?
        /// The bytes after the wrap, when the range crosses it.
        public let tail: Data?
    }

    /// Ring bytes.
    public let capacity: UInt64
    /// The data area's addresses in this process, for the check that a span reached Ghostty without a copy.
    public let dataAddresses: Range<UInt>

    /// The mapping (`headerByteCount + capacity` bytes, `PROT_READ`), stored once in init. A raw pointer is not
    /// Sendable, an atomic word is; every read is relaxed, one plain load.
    private let mapping: Atomic<UnsafeMutableRawPointer>
    private let mappedByteCount: Int
    private let endWord: AtomicPointerSlot
    private let reserveWord: AtomicPointerSlot
    private let baseWord: AtomicPointerSlot
    private let lapped = Atomic<Bool>(false)
    private let cutLaps = Atomic<UInt64>(0)
    private let releaseLaps = Atomic<UInt64>(0)
    /// The starts of live spans (``freeSlot`` when free), in memory the ring allocates once and frees in deinit.
    private let heldStorage: Atomic<UnsafeMutableRawPointer>
    private let heldSlots: [AtomicPointerSlot]
    /// Live spans that found no free slot.
    private let untracked = Atomic<Int>(0)
    /// The end of the last range cut; where the cursor goes when no span is live.
    private let cutEnd: Atomic<UInt64>
    /// The offset of the last ``takeAck()``.
    private let lastAck: Atomic<UInt64>

    /// Maps the ring `fd` names. Refuses a descriptor that maps writable, then maps it `PROT_READ, MAP_SHARED`
    /// and checks the header: `CMAR`, layout 1, a capacity in `1...maxCapacity` that the object holds, and an
    /// end not before the base. Takes `fd`: it is closed whether or not the ring opens (the mapping keeps the
    /// object alive).
    public init(adopting fd: Int32) throws(OpenError) {
        defer { Darwin.close(fd) }
        // SAFETY: a one-byte probe mapping of the descriptor, never written and unmapped before the throw.
        if let probe = unsafe mmap(nil, 1, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0),
           unsafe probe != UnsafeMutableRawPointer(bitPattern: -1) {
            unsafe munmap(probe, 1)
            throw .writable
        }
        var info = stat()
        // SAFETY: fstat writes one `stat` into `info` during the call.
        guard unsafe fstat(fd, &info) == 0 else { throw .statFailed(errno: errno) }
        let size = UInt64(max(info.st_size, 0))
        let headerBytes = UInt64(SharedOutputRing.headerByteCount)
        guard size >= headerBytes else { throw .tooSmall(size: size, needed: headerBytes) }

        let header = try SharedOutputRing.map(fd, byteCount: SharedOutputRing.headerByteCount)
        let fields = SharedOutputRing.readHeader(header)
        // SAFETY: the header mapping made above, unmapped once.
        unsafe munmap(header, SharedOutputRing.headerByteCount)
        guard fields.magic == SharedOutputRing.magic else { throw .badMagic }
        guard fields.layout == SharedOutputRing.layout else { throw .badLayout(fields.layout) }
        guard fields.capacity > 0, fields.capacity <= SharedOutputRing.maxCapacity else { throw .badCapacity(fields.capacity) }
        let needed = headerBytes + fields.capacity
        guard needed <= size else { throw .tooSmall(size: size, needed: needed) }
        guard fields.end >= fields.base else { throw .endBeforeBase(base: fields.base, end: fields.end) }

        // badCapacity bounded it by maxCapacity, so it fits an Int.
        let byteCount = Int(needed)
        let mapping = try SharedOutputRing.map(fd, byteCount: byteCount)
        self.mapping = unsafe Atomic(mapping)
        mappedByteCount = byteCount
        capacity = fields.capacity
        // SAFETY: the end, reserve and base words are 8-byte aligned (the mapping is page aligned) inside the
        // header, which the mapping holds; the slots are used only while `self` keeps the mapping.
        endWord = unsafe AtomicPointerSlot((mapping + SharedOutputRing.endOffset).assumingMemoryBound(to: UInt64.self))
        reserveWord = unsafe AtomicPointerSlot((mapping + SharedOutputRing.reserveOffset).assumingMemoryBound(to: UInt64.self))
        baseWord = unsafe AtomicPointerSlot((mapping + SharedOutputRing.baseOffset).assumingMemoryBound(to: UInt64.self))
        let held = UnsafeMutablePointer<UInt64>.allocate(capacity: SharedOutputRing.heldSlotCount)
        // SAFETY: `held` is a fresh allocation of heldSlotCount words, initialized here and freed once in deinit;
        // each slot covers one aligned word of it.
        unsafe held.initialize(repeating: SharedOutputRing.freeSlot, count: SharedOutputRing.heldSlotCount)
        heldStorage = unsafe Atomic(UnsafeMutableRawPointer(held))
        heldSlots = (0 ..< SharedOutputRing.heldSlotCount).map { index in unsafe AtomicPointerSlot(held + index) }
        cutEnd = Atomic(fields.end)
        lastAck = Atomic(fields.end)
        let dataStart = unsafe UInt(bitPattern: mapping + SharedOutputRing.headerByteCount)
        dataAddresses = dataStart ..< dataStart + UInt(fields.capacity)
    }

    deinit {
        // SAFETY: the mapping made in init; no span outlives `self` (each holds a reference), so nothing reads it.
        unsafe munmap(mapping.load(ordering: .relaxed), mappedByteCount)
        // SAFETY: the slot words allocated in init; no span is live (each holds `self`), so no slot is used.
        unsafe heldStorage.load(ordering: .relaxed).deallocate()
    }

    /// Offset after the last byte the writer published.
    public var end: UInt64 {
        UInt64(littleEndian: endWord.load(ordering: .acquiring))
    }

    /// Offset of the first byte the ring holds. It moves forward when the daemon skips output for a stream's
    /// cursor (a gap); read after ``end``, it is at least the base of every skip that end follows.
    public var base: UInt64 {
        UInt64(littleEndian: baseWord.load(ordering: .sequentiallyConsistent))
    }

    /// The bytes of `[from, to)` as no-copy spans of the mapping, after the reserve rule passed. Each span keeps
    /// the ring mapped until it is released, and checks the rule again then. A range that starts before the ring's
    /// base was skipped by the daemon (a gap): ``SpanError/beforeBase(from:base:)``, and the stream resyncs.
    /// One stream cuts at a time.
    public func cut(from: UInt64, to: UInt64) throws(SpanError) -> Spans {
        guard to >= from else { throw .backwards(from: from, to: to) }
        let length = to - from
        guard length > 0 else {
            try check(from: from, to: to, length: length)
            return Spans(range: from ..< to, head: nil, tail: nil)
        }
        // The head's slot is taken before the range is checked: an ack that did not see the slot read a base this
        // check sees too, so no span starts below an offset the daemon was told is free.
        let headSlot = claimSlot(start: from)
        do throws(SpanError) {
            try check(from: from, to: to, length: length)
        } catch {
            releaseSlot(headSlot)
            throw error
        }
        let at = from % capacity
        let first = min(length, capacity - at)
        let head = span(at: at, count: first, from: from, slot: headSlot)
        // The tail's slot is taken while the head's still holds the cursor below it.
        let tail = length > first
            ? span(at: 0, count: length - first, from: from + first, slot: claimSlot(start: from + first)) : nil
        advanceCutEnd(to: to)
        return Spans(range: from ..< to, head: head, tail: tail)
    }

    /// The offset to send in the stream's RING_ACK now, or nil when it did not move since the last one (or a live
    /// span is untracked): the start of the oldest live span, or, with none live, the end of the last cut and at
    /// least the ring's base, since after a gap the app holds nothing before it. Call it once per display frame:
    /// that coalesces the acks to one per frame.
    public func takeAck() -> UInt64? {
        // Base first, then the slots: a cut takes its slot before it reads the base (sequentially consistent).
        let base = self.base
        var floor = max(cutEnd.load(ordering: .sequentiallyConsistent), base)
        guard untracked.load(ordering: .sequentiallyConsistent) == 0 else { return nil }
        for slot in heldSlots {
            let start = slot.load(ordering: .sequentiallyConsistent)
            if start != Self.freeSlot { floor = min(floor, start) }
        }
        var previous = lastAck.load(ordering: .relaxed)
        while floor > previous {
            let (exchanged, found) = lastAck.compareExchange(expected: previous, desired: floor, ordering: .relaxed)
            if exchanged { return floor }
            previous = found
        }
        return nil
    }

    /// Whether a released span found that the writer lapped it while it was used; clears the mark.
    public func takeLapped() -> Bool {
        lapped.exchange(false, ordering: .acquiring)
    }

    /// Ranges ``cut(from:to:)`` refused as lapped.
    public var cutLapCount: UInt64 { cutLaps.load(ordering: .relaxed) }
    /// Spans whose release found a lap.
    public var releaseLapCount: UInt64 { releaseLaps.load(ordering: .relaxed) }

    private var reserve: UInt64 {
        UInt64(littleEndian: reserveWord.load(ordering: .relaxed))
    }

    /// Whether `[from, to)` may be read: inside what the ring holds, published, and not being overwritten. The end
    /// is read before the base: the daemon stores a skip's base before it publishes the end.
    private func check(from: UInt64, to: UInt64, length: UInt64) throws(SpanError) {
        let published = end
        let base = self.base
        guard from >= base else { throw .beforeBase(from: from, base: base) }
        guard to <= published else { throw .notPublished(to: to, end: published) }
        if length > capacity || overwrites(from: from) {
            cutLaps.add(1, ordering: .relaxed)
            throw .lapped(from: from, reserve: reserve)
        }
    }

    private func claimSlot(start: UInt64) -> HeldSlot {
        for slot in heldSlots
            where slot.compareExchange(expected: Self.freeSlot, desired: start, ordering: .sequentiallyConsistent).exchanged {
            return .slot(slot)
        }
        untracked.add(1, ordering: .sequentiallyConsistent)
        return .untracked
    }

    private func releaseSlot(_ held: HeldSlot) {
        switch held {
        case .slot(let slot): slot.store(Self.freeSlot, ordering: .sequentiallyConsistent)
        case .untracked: untracked.subtract(1, ordering: .sequentiallyConsistent)
        }
    }

    private func advanceCutEnd(to offset: UInt64) {
        var current = cutEnd.load(ordering: .relaxed)
        while offset > current {
            let (exchanged, found) = cutEnd.compareExchange(expected: current, desired: offset,
                                                            ordering: .sequentiallyConsistent)
            if exchanged { return }
            current = found
        }
    }

    /// The reserve rule: whether the writer may have overwritten a byte at or after `from`.
    private func overwrites(from: UInt64) -> Bool {
        let (limit, overflow) = from.addingReportingOverflow(capacity)
        return !overflow && reserve > limit
    }

    private func span(at offset: UInt64, count: UInt64, from start: UInt64, slot: HeldSlot) -> Data {
        // SAFETY: `offset + count <= capacity` (cut splits at the wrap), so the bytes lie in the data area of the
        // mapping. The deallocator holds `self`, which keeps the mapping until the span is released.
        unsafe Data(bytesNoCopy: mapping.load(ordering: .relaxed) + Self.headerByteCount + Int(offset), count: Int(count),
                    deallocator: .custom { _, _ in self.released(from: start, slot: slot) })
    }

    /// A span starting at `start` was used and dropped: the seqlock's second check, then its slot is free.
    private func released(from start: UInt64, slot: HeldSlot) {
        atomicFence(ordering: .acquiring)
        if overwrites(from: start) {
            releaseLaps.add(1, ordering: .relaxed)
            lapped.store(true, ordering: .releasing)
        }
        releaseSlot(slot)
    }

    private struct HeaderFields {
        var magic: [UInt8]
        var layout: UInt32
        var capacity: UInt64
        var base: UInt64
        var end: UInt64
    }

    private static func readHeader(_ header: UnsafeMutableRawPointer) -> HeaderFields {
        // SAFETY (this function): `header` is a live mapping of headerByteCount bytes, page aligned, so every
        // read below is inside it and aligned for its type.
        let magic = unsafe [UInt8](UnsafeRawBufferPointer(start: header, count: Self.magic.count))
        let layout = unsafe UInt32(littleEndian: header.load(fromByteOffset: layoutOffset, as: UInt32.self))
        let capacity = unsafe UInt64(littleEndian: header.load(fromByteOffset: capacityOffset, as: UInt64.self))
        let base = unsafe UInt64(littleEndian: header.load(fromByteOffset: baseOffset, as: UInt64.self))
        let endWord = unsafe AtomicPointerSlot((header + endOffset).assumingMemoryBound(to: UInt64.self))
        let end = UInt64(littleEndian: endWord.load(ordering: .acquiring))
        return HeaderFields(magic: magic, layout: layout, capacity: capacity, base: base, end: end)
    }

    private static func map(_ fd: Int32, byteCount: Int) throws(OpenError) -> UnsafeMutableRawPointer {
        // SAFETY: a read-only shared mapping of `byteCount` bytes; the caller checked that the object holds them
        // (or, for the header, at least headerByteCount) and unmaps it once.
        guard let mapping = unsafe mmap(nil, byteCount, PROT_READ, MAP_SHARED, fd, 0),
              unsafe mapping != UnsafeMutableRawPointer(bitPattern: -1) else { throw .mapFailed(errno: errno) }
        return unsafe mapping
    }
}
