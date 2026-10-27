import CmuxNextCompat
import Foundation

/// Whether the writer lapped ring bytes of one screen, and the one repair that answers it. One per screen: a
/// READY starts a new one and retires the old, so a late lap of the replaced screen's bytes asks for nothing; a
/// RING that replaces the mapping keeps it. Whoever finds a lap first (the queue at enqueue or pop, a span's
/// release check, or the decoder's own gap) claims it, and only the first claim asks for the fresh screen, at
/// once, without waiting for another frame (design section 3.4).
final class TerminalRingLaps: Sendable {
    private static let clean: UInt8 = 0
    private static let lapped: UInt8 = 1
    private static let retired: UInt8 = 2

    private let state = Atomic<UInt8>(TerminalRingLaps.clean)
    /// Asks for the fresh screen: `snapshot-request` on a snapshot stream, a reattach on a byte-replay one.
    private let repair: @Sendable () -> Void

    init(repair: @escaping @Sendable () -> Void) {
        self.repair = repair
    }

    /// Whether the screen was claimed lapped: its ring bytes are dropped until the fresh screen comes.
    var isMarked: Bool {
        state.load(ordering: .acquiring) == Self.lapped
    }

    /// Marks the screen lapped. True for the first claim of a current screen: that caller owes the repair
    /// (``requestRepair()``, run outside any lock it holds), or is the decoder, which asks itself.
    func claim() -> Bool {
        state.compareExchange(expected: Self.clean, desired: Self.lapped, ordering: .acquiringAndReleasing).exchanged
    }

    /// Claims the screen and, when this is the first claim, asks for the repair.
    func mark() {
        guard claim() else { return }
        repair()
    }

    /// Runs the repair a successful ``claim()`` owes.
    func requestRepair() {
        repair()
    }

    /// A READY replaced this screen: a lap of its bytes found later claims nothing.
    func retire() {
        state.compareExchange(expected: Self.clean, desired: Self.retired, ordering: .acquiringAndReleasing)
    }
}

/// PTY output `range` of a ring stream, cut by the ring: one run of the data area, never across the wrap, with
/// `bytes` pointing into the mapping (design section 4.4). The ring checked the reserve rule and the published
/// end when it cut them; releasing `bytes` checks the rule again and claims `laps` on a lap.
struct TerminalRingSpan: Sendable {
    let ring: any TerminalOutputRing
    /// The lap mark of the screen these bytes continue.
    let laps: TerminalRingLaps
    let range: Range<UInt64>
    let bytes: Data

    /// The span as a queued range: the queue cuts it again when the consumer takes it.
    var run: TerminalRingRun {
        TerminalRingRun(ring: ring, laps: laps, range: range)
    }
}

/// Ring output as a range of the mapping, not bytes: what a view's queue holds. It is cut from the ring when
/// the consumer takes it, so the ring checks the reserve rule again and a range lapped while it waited never
/// reaches the parser.
struct TerminalRingRun: Sendable {
    let ring: any TerminalOutputRing
    let laps: TerminalRingLaps
    let range: Range<UInt64>

    var count: UInt64 { range.upperBound - range.lowerBound }

    /// `self` and `next` as one run when `next` continues it in the PTY stream and in the mapping: the same
    /// ring and screen, and no wrap between them. Nil otherwise.
    func joined(with next: TerminalRingRun) -> TerminalRingRun? {
        guard next.ring === ring, next.laps === laps, next.range.lowerBound == range.upperBound,
              range.upperBound % ring.capacity != 0
        else { return nil }
        return TerminalRingRun(ring: ring, laps: laps, range: range.lowerBound ..< next.range.upperBound)
    }

    /// Whether the writer has overwritten, or is overwriting, the run's first byte by now (the ring's rule).
    var isLapped: Bool {
        do throws(TerminalRingCutError) {
            try Self.checkWhole(ring, from: range.lowerBound)
            return false
        } catch {
            return true
        }
    }

    /// The run's bytes, cut from the ring now: one span, or two when the range crosses the wrap; nil for an
    /// empty range. Each span's release checks the reserve rule again and claims `laps` on a lap, on whatever
    /// thread drops it, so a lap during the parse asks for the fresh screen even when no frame follows.
    func cut() throws(TerminalRingCutError) -> (head: TerminalRingSpan, tail: TerminalRingSpan?)? {
        let cut = try ring.cut(from: range.lowerBound, to: range.upperBound)
        guard let first = cut.head else { return nil }
        let head = try span(first, from: range.lowerBound)
        guard let rest = cut.tail else { return (head, nil) }
        return (head, try span(rest, from: head.range.upperBound))
    }

    /// `bytes`, the ring's span from `start`, as `Data` at the same address in the mapping whose release checks
    /// the rule for this span's screen. The closure holds the ring, which keeps the mapping.
    private func span(_ bytes: Data, from start: UInt64) throws(TerminalRingCutError) -> TerminalRingSpan {
        let ring = ring
        let laps = laps
        let range = start ..< start + UInt64(bytes.count)
        // SAFETY: only the address is read, inside the closure that lends it.
        let address = unsafe bytes.withUnsafeBytes { buffer in unsafe UInt(bitPattern: buffer.baseAddress) }
        guard ring.dataAddresses.contains(address),
              let pointer = unsafe UnsafeMutableRawPointer(bitPattern: address)
        else {
            // `Data` copied a span this small inline inside the cut: this check is the copy's release check.
            try Self.checkWhole(ring, from: start)
            return TerminalRingSpan(ring: ring, laps: laps, range: range, bytes: bytes)
        }
        // SAFETY: `bytes.count` bytes from `pointer` lie in the read-only mapping (the ring cut them there), and the
        // deallocator holds `ring`, which keeps the mapping until it runs; nothing writes through the pointer.
        let checked = unsafe Data(bytesNoCopy: pointer, count: bytes.count, deallocator: .custom { _, _ in
            do throws(TerminalRingCutError) {
                try Self.checkWhole(ring, from: start)
            } catch {
                laps.mark()
            }
        })
        return TerminalRingSpan(ring: ring, laps: laps, range: range, bytes: checked)
    }

    /// The seqlock's second check, through the ring's own rule: an empty cut at `start` after every byte read
    /// before it (acquire fence), refused as lapped once the writer reserved past `start + capacity`. Only
    /// `.lapped` is thrown: the cut that made the span already passed the published-end and base checks.
    private static func checkWhole(_ ring: any TerminalOutputRing, from start: UInt64) throws(TerminalRingCutError) {
        atomicFence(ordering: .acquiring)
        do throws(TerminalRingCutError) {
            _ = try ring.cut(from: start, to: start)
        } catch {
            guard case .lapped = error else { return }
            throw error
        }
    }
}
