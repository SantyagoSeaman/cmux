import Foundation

/// The read side of one terminal's output ring that a ring stream uses (`terminal-output-ring-v1`, CMAR
/// layout 1; plans/cmux-next/app-daemon-connection-DESIGN.md section 4). The byte at PTY offset `o` lies at
/// `data[o % capacity]`. The daemon's single writer stores `reserve` before it copies a chunk and `end` after
/// it, so a reader that loads `reserve` after it read `[s, t)` knows whether the writer may have overwritten
/// it (the seqlock rule). `SharedOutputRing` (CmuxNextIPC, p05-ipc-112) is the read-only mapping; tests use
/// an in-process one. Mojo hands a data pipe's consumer the shared buffer the same way, a two-phase read in
/// place (mojo/public/c/system/data_pipe.h, `MojoBeginReadData`).
protocol TerminalOutputRing: AnyObject, Sendable {
    /// Bytes of the data area.
    var capacity: UInt64 { get }
    /// The `reserve` word after an acquire fence: every byte read before the call was read before this load.
    func reserveAfterReading() -> UInt64
    /// `count` bytes from PTY offset `start`, in one run of the data area
    /// (`start % capacity + count <= capacity`), as `Data` that points into the mapping. The mapping stays
    /// until the ring and every such `Data` are gone; `released` runs once, when this `Data`'s storage goes.
    /// Foundation keeps a `Data` of a few bytes inline: it copies them at once and releases the mapping's
    /// bytes then, so `released` checks that copy, which is the same seqlock read.
    func bytesNoCopy(from start: UInt64, count: Int, released: @escaping @Sendable () -> Void) -> Data
}

/// Maps the read-only descriptor of a RING frame for `grant` (p05-ipc-112's `SharedOutputRing`: it refuses a
/// descriptor that maps writable and checks the header against the grant). It owns `descriptor` from the
/// call on and closes it whether it maps or throws.
typealias TerminalRingMapper = @Sendable (_ descriptor: Int32, _ grant: TerminalRingGrant)
    throws(TerminalRingRefusal) -> any TerminalOutputRing
