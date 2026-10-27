// How the terminals' output lanes read ring output, for `debug.terminal`.
extension TerminalSession {
    /// The live surface's lane counts (``TerminalRingReads``); nil before the surface exists. A surface a
    /// replay swaps in starts a new lane at zero.
    public var ringReads: TerminalRingReads? { surfaceView.lane?.ringReads }
}

/// `debug.terminal`: ``TerminalRingReads`` summed over the live terminals' lanes. `copies_per_chunk` is at most
/// 1. A `.ringOutput` carries only ring offsets, so the path has no base64 or JSON stage: `base64_decodes` and
/// `json_decodes` say so with 0.
public nonisolated struct TerminalRingReadsReport: Sendable, Equatable {
    public let lanes: Int
    public let reads: TerminalRingReads

    public init(_ perLane: [TerminalRingReads]) {
        lanes = perLane.count
        reads = perLane.reduce(.zero, +)
    }

    @MainActor public init(sessions: [TerminalSession]) {
        self.init(sessions.compactMap(\.ringReads))
    }

    /// The reply as a `JSONSerialization` graph: `lanes`, and `ring` with `chunks`, `copies`, `bytes_copied`,
    /// `laps`, `dropped`, `copies_per_chunk`, `base64_decodes`, `json_decodes`.
    public var foundationObject: [String: Any] {
        let copiesPerChunk = reads.chunks == 0 ? 0 : Double(reads.copies) / Double(reads.chunks)
        return [
            "lanes": Double(lanes),
            "ring": [
                "chunks": Double(reads.chunks), "copies": Double(reads.copies),
                "bytes_copied": Double(reads.bytesCopied), "laps": Double(reads.laps),
                "dropped": Double(reads.dropped), "copies_per_chunk": copiesPerChunk,
                "base64_decodes": 0.0, "json_decodes": 0.0,
            ] as [String: Any],
        ]
    }
}
