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
public final class SharedOutputRing: Sendable {
    /// Bytes before the data area.
    public static let headerByteCount = 64
    /// The layout this build reads.
    public static let layout: UInt32 = 1
    /// The largest capacity a header may claim: a damaged header must not make the app map gigabytes.
    public static let maxCapacity: UInt64 = 64 << 20

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
    /// Offset of the first byte this ring held.
    public let base: UInt64
    /// The data area's addresses in this process, for the check that a span reached Ghostty without a copy.
    public let dataAddresses: Range<UInt>

    /// The mapping (`headerByteCount + capacity` bytes, `PROT_READ`), stored once in init. A raw pointer is not
    /// Sendable, an atomic word is; every read is relaxed, one plain load.
    private let mapping: Atomic<UnsafeMutableRawPointer>
    private let mappedByteCount: Int
    private let endWord: AtomicPointerSlot
    private let reserveWord: AtomicPointerSlot
    private let lapped = Atomic<Bool>(false)
    private let cutLaps = Atomic<UInt64>(0)
    private let releaseLaps = Atomic<UInt64>(0)

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
        base = fields.base
        // SAFETY: the end and reserve words are 8-byte aligned (the mapping is page aligned) inside the header,
        // which the mapping holds; the slots are used only while `self` keeps the mapping.
        endWord = unsafe AtomicPointerSlot((mapping + SharedOutputRing.endOffset).assumingMemoryBound(to: UInt64.self))
        reserveWord = unsafe AtomicPointerSlot((mapping + SharedOutputRing.reserveOffset).assumingMemoryBound(to: UInt64.self))
        let dataStart = unsafe UInt(bitPattern: mapping + SharedOutputRing.headerByteCount)
        dataAddresses = dataStart ..< dataStart + UInt(fields.capacity)
    }

    deinit {
        // SAFETY: the mapping made in init; no span outlives `self` (each holds a reference), so nothing reads it.
        unsafe munmap(mapping.load(ordering: .relaxed), mappedByteCount)
    }

    /// Offset after the last byte the writer published.
    public var end: UInt64 {
        UInt64(littleEndian: endWord.load(ordering: .acquiring))
    }

    /// The bytes of `[from, to)` as no-copy spans of the mapping, after the reserve rule passed. Each span keeps
    /// the ring mapped until it is released, and checks the rule again then.
    public func cut(from: UInt64, to: UInt64) throws(SpanError) -> Spans {
        guard to >= from else { throw .backwards(from: from, to: to) }
        guard from >= base else { throw .beforeBase(from: from, base: base) }
        let published = end
        guard to <= published else { throw .notPublished(to: to, end: published) }
        let length = to - from
        if length > capacity || overwrites(from: from) {
            cutLaps.add(1, ordering: .relaxed)
            throw .lapped(from: from, reserve: reserve)
        }
        guard length > 0 else { return Spans(range: from ..< to, head: nil, tail: nil) }
        let at = from % capacity
        let first = min(length, capacity - at)
        let head = span(at: at, count: first, from: from)
        let tail = length > first ? span(at: 0, count: length - first, from: from + first) : nil
        return Spans(range: from ..< to, head: head, tail: tail)
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

    /// The reserve rule: whether the writer may have overwritten a byte at or after `from`.
    private func overwrites(from: UInt64) -> Bool {
        let (limit, overflow) = from.addingReportingOverflow(capacity)
        return !overflow && reserve > limit
    }

    private func span(at offset: UInt64, count: UInt64, from start: UInt64) -> Data {
        // SAFETY: `offset + count <= capacity` (cut splits at the wrap), so the bytes lie in the data area of the
        // mapping. The deallocator holds `self`, which keeps the mapping until the span is released.
        unsafe Data(bytesNoCopy: mapping.load(ordering: .relaxed) + Self.headerByteCount + Int(offset), count: Int(count),
                    deallocator: .custom { _, _ in self.released(from: start) })
    }

    /// A span starting at `start` was used and dropped: the seqlock's second check.
    private func released(from start: UInt64) {
        atomicFence(ordering: .acquiring)
        guard overwrites(from: start) else { return }
        releaseLaps.add(1, ordering: .relaxed)
        lapped.store(true, ordering: .releasing)
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
