import Foundation

/// The read side of one terminal's output ring that a ring stream uses (`terminal-output-ring-v1`, CMAR
/// layout 1; plans/cmux-next/app-daemon-connection-DESIGN.md section 4). It has the shape of p05-ipc-112's
/// `SharedOutputRing` (CmuxNextIPC): ``cut(from:to:)`` is its `cut`, ``TerminalRingCut`` its `Spans` and
/// ``TerminalRingCutError`` its `SpanError`, case for case, so the production mapper conforms it with a
/// one-to-one mapping of those types. The ring owns the seqlock rule and the published-end check: the stream
/// cuts every range through it, and checks a span again at its release with an empty cut at the span's start,
/// so the lap is claimed for that span's screen (``TerminalRingRun/cut()``) rather than read back from the
/// ring's one `takeLapped` mark. Tests use an in-process ring. Mojo hands a data pipe's consumer the shared
/// buffer the same way, a two-phase read in place (mojo/public/c/system/data_pipe.h, `MojoBeginReadData`).
protocol TerminalOutputRing: AnyObject, Sendable {
    /// Bytes of the data area.
    var capacity: UInt64 { get }
    /// The data area's addresses in this process: a span's bytes lie here unless `Data` copied them inline.
    var dataAddresses: Range<UInt> { get }
    /// The bytes of `[from, to)` as spans that point into the mapping (two when the range crosses the wrap),
    /// after the reserve rule passed; an empty range checks the rule and gives no span. Refuses a range past
    /// the published end. The mapping stays while the ring or any span is alive.
    func cut(from: UInt64, to: UInt64) throws(TerminalRingCutError) -> TerminalRingCut
}

/// The bytes of one cut range, in stream order (`SharedOutputRing.Spans`).
struct TerminalRingCut: Sendable {
    /// The stream offsets the spans cover.
    let range: Range<UInt64>
    /// The bytes from `range.lowerBound`; nil for an empty range.
    let head: Data?
    /// The bytes after the wrap, when the range crosses it.
    let tail: Data?
}

/// Why a range may not be cut (`SharedOutputRing.SpanError`).
enum TerminalRingCutError: Error, Sendable, Equatable {
    /// The writer has overwritten, or may be overwriting, part of the range: a gap.
    case lapped(from: UInt64, reserve: UInt64)
    /// The range ends past the last published byte.
    case notPublished(to: UInt64, end: UInt64)
    /// The range starts before the first byte this ring held.
    case beforeBase(from: UInt64, base: UInt64)
    /// The range ends before it starts.
    case backwards(from: UInt64, to: UInt64)
}

/// Maps the read-only descriptor of a RING frame for `grant` (p05-ipc-112's `SharedOutputRing`: it refuses a
/// descriptor that maps writable and checks the header against the grant). It owns `descriptor` from the
/// call on and closes it whether it maps or throws.
typealias TerminalRingMapper = @Sendable (_ descriptor: Int32, _ grant: TerminalRingGrant)
    throws(TerminalRingRefusal) -> any TerminalOutputRing
