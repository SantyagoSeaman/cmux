/// Where a ring view's screen is when its stream ends: the PTY offset after the last byte it parsed and the
/// grid generation of that screen (its last READY's). A reattach names it in `attach-surface`
/// (`resume_offset`, `resume_generation`; terminal-output-ring-v1, p05-ipc-114). When the terminal's ring
/// still holds every byte from the offset and the grid is still at the generation, the daemon skips the READY:
/// the new stream is RING, then RING_ADVANCE from the offset. Otherwise the stream starts with a READY, as
/// without one. Chromium resumes an interrupted download the same way, from the bytes it has while the
/// server's validators still match (components/download/internal/common/download_item_impl.cc,
/// `ResumeInterruptedDownload`); the generation is the validator here.
public struct TerminalRingResume: Sendable, Hashable {
    /// The capability `attach-surface` needs to take a resume.
    static let capability = "terminal-output-ring-v1"

    /// The PTY offset after the last byte the view parsed.
    public var offset: UInt64
    /// The grid generation of the view's screen.
    public var generation: UInt64

    public init(offset: UInt64, generation: UInt64) {
        self.offset = offset
        self.generation = generation
    }
}
