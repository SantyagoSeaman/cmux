public import Foundation

/// The daemon's output ring of one attach stream, as the app reads it (`terminal-output-ring-v1`: cmux-tui
/// `surface/output_ring.rs` writes it, CmuxNextDaemon `TerminalOutputRingMapping` maps it read-only;
/// plans/cmux-next/app-daemon-connection-DESIGN.md section 4). The byte at stream offset `o` is at
/// `o % capacity` of ``dataArea``.
///
/// The daemon's one writer never waits for a reader and may overwrite any byte at any time, so bytes are
/// read only through ``TerminalRingChunk/read()``: copy first, then check ``oldestIntact()``.
public nonisolated protocol TerminalOutputRing: AnyObject, Sendable {
    /// Data bytes in the ring.
    var capacity: UInt64 { get }
    /// The offset of the first byte the ring ever held.
    var base: UInt64 { get }
    /// The ring's `capacity` data bytes, mapped as long as the ring lives.
    var dataArea: UnsafeRawBufferPointer { get }
    /// The oldest offset the writer has not started to overwrite: an acquire fence, then the ring's
    /// `reserve` word less `capacity` (`OutputRingReader::unlapped` in output_ring.rs is the same check).
    func oldestIntact() -> UInt64
    /// The writer overwrote `chunk` before the lane read it: the stream asks for a fresh READY
    /// (`snapshot-request`, reason gap) and cuts the chunks after that READY with `chunk.resync + 1`.
    /// Called on the output lane, once per lap: it must not block.
    func lapped(_ chunk: TerminalRingChunk)
}

/// Live output that sits in a stream's ``TerminalOutputRing`` as `[start, end)` (a RING_ADVANCE), handed to
/// the surface's output lane (``TerminalIOEvent/ringOutput(_:)``), which reads it once right before the
/// parse.
public nonisolated struct TerminalRingChunk: Sendable, Equatable {
    public let ring: any TerminalOutputRing
    public let start: UInt64
    public let end: UInt64
    /// The stream's resync count when it cut the chunk: the lane drops every chunk cut before the READY
    /// that answers a lap (``TerminalOutputRing/lapped(_:)``).
    public let resync: UInt64

    public init(ring: any TerminalOutputRing, start: UInt64, end: UInt64, resync: UInt64) {
        self.ring = ring
        self.start = start
        self.end = max(start, end)
        self.resync = resync
    }

    /// Bytes in the chunk (what the lane's backlog counts).
    var byteCount: Int { Int(clamping: end - start) }

    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.ring === rhs.ring && lhs.start == rhs.start && lhs.end == rhs.end && lhs.resync == rhs.resync
    }

    /// What ``read()`` got.
    nonisolated enum Read: Equatable {
        /// The chunk's bytes, copied out of the ring once and intact after the copy.
        case copied(Data)
        /// The writer overwrote part of the chunk (before or during the copy), or it was never in the ring:
        /// nothing of it may be parsed.
        case lapped
    }

    /// Copy, then validate: one copy of `[start, end)` out of the ring (two spans when it wraps), then the
    /// oldest intact offset again, with acquire. Bytes the writer started to overwrite during the copy are
    /// dropped whole, so a torn copy never reaches the parser. Chromium reads its one-writer shared memory the
    /// same way (`device/base/synchronization/one_writer_seqlock.h`: ReadBegin, copy, ReadRetry), and a Mojo
    /// data pipe consumer copies a wrapped ring in two spans (`mojo/core/data_pipe_consumer_dispatcher.cc`).
    // perf-hot: 1ms terminal-ring-read
    func read() -> Read {
        guard end > start else { return .copied(Data()) }
        let capacity = ring.capacity
        guard start >= ring.base, end - start <= capacity, ring.oldestIntact() <= start,
              let count = Int(exactly: end - start), let capacityBytes = Int(exactly: capacity),
              let at = Int(exactly: start % capacity)
        else { return .lapped }
        // SAFETY: the ring maps `capacity` data bytes while it lives and `ring` holds it. The copy is this
        // function's own allocation: the Data that wraps it frees it, or the torn path below does.
        let area = unsafe ring.dataArea
        // SAFETY: as above.
        guard unsafe area.count >= capacityBytes, let source = unsafe area.baseAddress else { return .lapped }
        // SAFETY: as above.
        let copy = unsafe UnsafeMutableRawPointer.allocate(byteCount: count, alignment: 1)
        let first = min(count, capacityBytes - at)
        // SAFETY: `[at, at + first)` and `[0, count - first)` lie inside the `capacity` data bytes, and the
        // copy holds `count` bytes.
        unsafe copy.copyMemory(from: source + at, byteCount: first)
        // SAFETY: as above; the wrapped span starts at the ring's first data byte.
        if first < count { unsafe (copy + first).copyMemory(from: source, byteCount: count - first) }
        guard ring.oldestIntact() <= start else {
            // SAFETY: as above, not wrapped by any Data.
            unsafe copy.deallocate()
            return .lapped
        }
        // SAFETY: the Data takes the allocation and frees it once (a short copy Foundation keeps inline frees it
        // at once).
        return .copied(unsafe Data(bytesNoCopy: copy, count: count, deallocator: .custom { pointer, _ in
            // SAFETY: the allocation above, freed once.
            unsafe pointer.deallocate()
        }))
    }
}
