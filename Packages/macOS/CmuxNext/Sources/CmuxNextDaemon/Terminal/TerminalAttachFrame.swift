import Foundation

/// One attach frame of a stream (cmux-tui `server/attach_frames.rs` and `server/snapshot_frames.rs`;
/// p05-ipc.md section 3.2): its kind, the absolute PTY offset of its aux, and its payload in the read buffer.
struct TerminalAttachFrame: Sendable {
    /// The attach kinds of the frame header (cmux-ipc `FrameKind`).
    enum Kind: UInt8, Sendable {
        /// PTY bytes; the aux is the offset of the first one.
        case output = 1
        /// A grid replacement (`resized`), `meta_len u32 | meta | replay | pending`, at the current offset.
        case grid = 2
        /// An attach event's JSON line at the current offset; an `output` sidecar (colors, flow) carries
        /// the offset of the OUTPUT it follows.
        case event = 3
        /// The first chunk of a `snapshot` event, `meta_len u32 | meta | data`, at its READY's offset.
        case snapshot = 4
        /// A later chunk of the SNAPSHOT before it, with the same aux.
        case snapshotMore = 5
        /// A `vt-state` replay, `meta_len u32 | meta | replay | pending`; every chunk has the cut's offset.
        case replay = 6
        /// A terminal's output ring, JSON meta `{capacity, base, layout}` with its read-only descriptor; the aux
        /// is the ring's end when sent (terminal-output-ring-v1, p05-ipc-113).
        case ring = 7
        /// Empty: the ring's bytes up to the aux (its new end) are published.
        case ringAdvance = 8
    }

    var kind: Kind
    var offset: UInt64
    var payload: TerminalFrameBytes
    /// The descriptor that rode on this frame (`SCM_RIGHTS`); only a RING has one.
    var descriptor: TerminalRingDescriptor? = nil
}

/// What one attach frame gives the view, in stream order (``TerminalAttachment/FrameDecoder``).
enum TerminalFrameAdmission: Sendable {
    /// Nothing: a chunk of an unfinished snapshot or replay, bytes the screen already has, an event views
    /// ignore, or a frame dropped while the stream waits for a fresh screen.
    case nothing
    /// Live PTY bytes, still in the read buffer.
    case output(TerminalOutputSpan)
    /// Live PTY bytes, still in the output ring: one span, or two when they wrap (a RING_ADVANCE).
    case ringOutput(TerminalRingSpan, wrapped: TerminalRingSpan?)
    /// Any other event; `flow` names the key press that the output just before it echoes.
    case event(TerminalChannelEvent, flow: UInt64?)
    /// Bytes were lost: ask the host for a fresh READY (`snapshot-request`); screen bytes are dropped until it comes.
    case resync(SnapshotRequestReason)
    /// A RING or RING_ADVANCE the stream cannot use: a protocol error that ends the connection.
    case refused(TerminalRingRefusal)
}
