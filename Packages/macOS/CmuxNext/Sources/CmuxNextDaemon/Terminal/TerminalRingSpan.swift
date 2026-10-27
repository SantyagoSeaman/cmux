import CmuxNextCompat
import Foundation

/// Whether the writer lapped a screen's ring bytes after they were cut: set by the release check of a span
/// that was parsed while the writer overwrote it, and by the queue when a span was lapped before the view took
/// it. The stream's next frame then asks for a fresh READY. One per screen: a READY starts a new one, so a late
/// release of a span of the replaced screen never marks the new one.
final class TerminalRingLaps: Sendable {
    private let marked = Atomic<Bool>(false)

    init() {}

    func mark() {
        marked.store(true, ordering: .releasing)
    }

    var isMarked: Bool {
        marked.load(ordering: .acquiring)
    }
}

/// PTY output `offset ..< end` of a ring stream, still in the ring: one run of the data area, never across
/// the wrap (design section 4.4).
struct TerminalRingSpan: Sendable {
    let ring: any TerminalOutputRing
    /// The lap mark of the screen these bytes continue.
    let laps: TerminalRingLaps
    let offset: UInt64
    let count: Int

    /// The offset just past the last byte.
    var end: UInt64 { offset &+ UInt64(truncatingIfNeeded: count) }

    /// The bytes of `[start, end)` cut from `ring` after `reserve` was loaded: one span, or two when the range
    /// wraps. Nil when bytes of it are gone or may be being overwritten: more than the capacity behind, or the
    /// writer started past `start + capacity` (the check at cut, design section 4.4).
    static func cut(_ ring: any TerminalOutputRing, laps: TerminalRingLaps, from start: UInt64, to end: UInt64,
                    reserve: UInt64) -> (head: TerminalRingSpan, wrapped: TerminalRingSpan?)? {
        let capacity = ring.capacity
        guard capacity > 0, end > start, end - start <= capacity,
              !lapped(start: start, capacity: capacity, reserve: reserve),
              let total = Int(exactly: end - start), let first = Int(exactly: min(end - start, capacity - start % capacity))
        else { return nil }
        let head = TerminalRingSpan(ring: ring, laps: laps, offset: start, count: first)
        guard first < total else { return (head, nil) }
        return (head, TerminalRingSpan(ring: ring, laps: laps, offset: head.end, count: total - first))
    }

    /// The seqlock rule: the writer may have overwritten the byte at `start` once it started a byte past
    /// `start + capacity`. A `reserve` behind `start` is a broken ring, so it counts as lapped too.
    static func lapped(start: UInt64, capacity: UInt64, reserve: UInt64) -> Bool {
        reserve < start || reserve - start > capacity
    }

    /// Whether the writer may have overwritten bytes of this span by now.
    var isLapped: Bool {
        laps.isMarked || Self.lapped(start: offset, capacity: ring.capacity, reserve: ring.reserveAfterReading())
    }

    /// `self` and `next` as one span when `next` continues it in the PTY stream and in the mapping: the same
    /// ring and screen, and no wrap between them. Nil otherwise.
    func joined(with next: TerminalRingSpan) -> TerminalRingSpan? {
        guard next.ring === ring, next.laps === laps, next.offset == end, end % ring.capacity != 0 else { return nil }
        return TerminalRingSpan(ring: ring, laps: laps, offset: offset, count: count + next.count)
    }

    /// ``data()``, unless the writer lapped the span before the view took it: then nil, and its screen is marked
    /// lapped so the stream resyncs.
    func dataUnlessLapped() -> Data? {
        guard !isLapped else {
            laps.mark()
            return nil
        }
        return data()
    }

    /// The bytes for the view, pointing into the mapping. When the view drops them (Ghostty parsed them), the
    /// rule is checked again: a writer that lapped them during the parse marks the screen lapped.
    func data() -> Data {
        let span = self
        return ring.bytesNoCopy(from: offset, count: count) {
            guard Self.lapped(start: span.offset, capacity: span.ring.capacity,
                              reserve: span.ring.reserveAfterReading()) else { return }
            span.laps.mark()
        }
    }
}
