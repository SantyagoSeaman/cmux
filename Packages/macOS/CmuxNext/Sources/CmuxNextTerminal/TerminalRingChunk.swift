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

    /// Copy, then validate: one copy of `[start, end)` out of the ring straight into the chunk's `Data` (two
    /// spans when it wraps; a keystroke echo stays in the `Data`'s inline bytes, with no allocation), then the
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
        // SAFETY: the ring maps `capacity` data bytes while it lives and `ring` holds it.
        let area = unsafe ring.dataArea
        // SAFETY: as above.
        guard unsafe area.count >= capacityBytes, let source = unsafe area.baseAddress else { return .lapped }
        // SAFETY: as above; the data area holds bytes, so binding it to UInt8 for the copy is sound.
        let bytes = unsafe source.assumingMemoryBound(to: UInt8.self)
        let first = min(count, capacityBytes - at)
        var data = Data(capacity: count)
        // SAFETY: `[at, at + first)` and `[0, count - first)` lie inside the `capacity` data bytes; `append`
        // copies them into `data`'s own storage before it returns.
        unsafe data.append(bytes + at, count: first)
        // SAFETY: as above; the wrapped span starts at the ring's first data byte.
        if first < count { unsafe data.append(bytes, count: count - first) }
        guard ring.oldestIntact() <= start else { return .lapped }
        return .copied(data)
    }
}

/// How output lanes read ring output (`debug.terminal`): one copy per chunk parsed, none for a chunk that
/// lapped or was dropped. A `.ringOutput` carries only ring offsets, so the path has no base64 or JSON stage.
public nonisolated struct TerminalRingReads: Sendable, Equatable {
    /// Ring chunks the lane took off its queue (closed lanes skip theirs uncounted).
    public var chunks: UInt64
    /// Chunks copied out of the ring and parsed.
    public var copies: UInt64
    public var bytesCopied: UInt64
    /// Chunks the writer overwrote before or during the copy: each told its stream once.
    public var laps: UInt64
    /// Chunks cut before the READY that answers a lap, dropped unread.
    public var dropped: UInt64

    public init(chunks: UInt64, copies: UInt64, bytesCopied: UInt64, laps: UInt64, dropped: UInt64) {
        self.chunks = chunks
        self.copies = copies
        self.bytesCopied = bytesCopied
        self.laps = laps
        self.dropped = dropped
    }

    public static let zero = Self(chunks: 0, copies: 0, bytesCopied: 0, laps: 0, dropped: 0)

    /// The counts of two lanes together.
    public static func + (lhs: Self, rhs: Self) -> Self {
        Self(chunks: lhs.chunks + rhs.chunks, copies: lhs.copies + rhs.copies,
             bytesCopied: lhs.bytesCopied + rhs.bytesCopied, laps: lhs.laps + rhs.laps,
             dropped: lhs.dropped + rhs.dropped)
    }
}
