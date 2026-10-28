/// The `attach-surface` request of one view, with only what the host supports.
extension TerminalAttachment {
    /// What one view asks of its attach.
    struct AttachOptions: Sendable {
        var snapshotVersion: UInt16?
        var localHistory: Bool
        var images: Bool
        /// A reattaching ring view's screen (p05-ipc-114): sent to a host with ``TerminalRingResume/capability``
        /// on a snapshot attach, so the stream skips the READY while the ring still holds the offset.
        var resume: TerminalRingResume?
    }

    /// The request for `target` on a host with `identity`; `useIdentity` names the terminal by its public id.
    static func attachRequest(target: Target, useIdentity: Bool, identity: DaemonIdentity, size: CellSize,
                              options: AttachOptions) -> AttachSurfaceRequest {
        AttachSurfaceRequest(
            surface: useIdentity ? nil : target.surface,
            expectedGeneration: useIdentity ? target.generation : nil,
            expectedTerminalID: useIdentity ? target.terminalResourceID : nil,
            size: size,
            snapshotVersion: identity.supports(snapshotCapability) ? options.snapshotVersion : nil,
            snapshotLocalHistory: options.localHistory
                && identity.supports(DaemonCapability.terminalSnapshotLocalHistory.rawValue),
            snapshotImages: options.images && identity.supports(DaemonCapability.terminalSnapshotImages.rawValue),
            resume: identity.supports(TerminalRingResume.capability) ? options.resume : nil
        )
    }
}
