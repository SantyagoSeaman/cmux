public import Foundation
import CmuxNextCompat
public import CmuxNextTrace
import CmuxNextWakeups
import os

/// One attached terminal view: a client stream (`attach-streams-v1`) on the
/// carrier its route gives (``TerminalStreamCarrier``), the machine's one
/// connection to its daemon or, while `daemonTerminalStreams` is off, a
/// connection of the view's own that carries only this stream. Output flows
/// through the view's bounded ``TerminalEventQueue``; after the attach every
/// command here is fire-and-forget. Views share one connection as Chrome's
/// associated interfaces share one message pipe, each with its own id and
/// queue (mojo/public/cpp/bindings/lib/multiplex_router.cc). The connection's
/// reader never waits for this view: a view that falls behind loses its
/// queued output and resyncs by snapshot (D5).
public actor TerminalAttachment: TerminalByteChannel {
    public struct Target: Sendable, Hashable {
        public var surface: SurfaceID
        /// With `attach-identity-v1` the daemon resolves the terminal by its
        /// public resource id (`term_…`, the tab's `terminal_resource_id`,
        /// not the 32-hex `terminal_id`) and validates the generation instead
        /// of trusting a possibly stale numeric surface.
        public var terminalResourceID: ResourceID?
        public var generation: DaemonGeneration?

        public init(surface: SurfaceID, terminalResourceID: ResourceID? = nil, generation: DaemonGeneration? = nil) {
            self.surface = surface
            self.terminalResourceID = terminalResourceID
            self.generation = generation
        }

        public init(tab: TabSnapshot, generation: DaemonGeneration?) {
            self.init(surface: tab.surface, terminalResourceID: tab.terminalResourceID, generation: generation)
        }

        /// A terminal with no tab on its session (its view lives in another
        /// session's layout): attached by `term_` id and generation only;
        /// the first `vt-state` names its surface.
        public static func unplaced(terminalResourceID: ResourceID, generation: DaemonGeneration) -> Target {
            Target(surface: unresolvedSurface, terminalResourceID: terminalResourceID, generation: generation)
        }
    }

    /// The surface of an `unplaced` target until its `vt-state` names one.
    public static let unresolvedSurface = SurfaceID(rawValue: 0)

    public nonisolated let events: AsyncStream<TerminalChannelEvent>
    /// The attached surface. For an `unplaced` target it is
    /// `unresolvedSurface` until the first `vt-state`, which the daemon
    /// sends before the attach reply, so every later command has it.
    public nonisolated var surface: SurfaceID { resolvedSurface.value.withLock { $0 } }
    private nonisolated let resolvedSurface: SurfaceBox

    final class SurfaceBox: Sendable {
        let value: Mutex<SurfaceID>
        init(_ surface: SurfaceID) { value = Mutex(surface) }
    }

    /// The carrier's connection; this view only sends on it, tagged with ``wireStream``.
    private nonisolated let transport: LineTransport
    /// The connection is this view's own (``TerminalStreamCarrier/ownsConnection``).
    private nonisolated let ownsConnection: Bool
    /// The view's client stream on its carrier.
    nonisolated let stream: UInt16
    /// The stream id the view's requests name: ``stream``, or nil on a connection of its own to a
    /// daemon without `attach-streams-v1` (``TerminalStreamCarrier/tagsStreams``).
    private nonisolated let wireStream: UInt16?
    private nonisolated let queue: TerminalEventQueue
    private nonisolated let logger = Logger(subsystem: "com.cmuxterm.app.next", category: "daemon.attach")

    /// Written by the attach handshake, then read by every command. Guarded
    /// so the synchronous command path can run from any thread in caller
    /// order; no lock is held across a send.
    struct Control: Sendable {
        var lease: String?
        var lastReported: CellSize?
        var detached = false
        /// The daemon takes `set-size-counts` for this lease (`shared-sizing-v1`).
        var hidesInPlace = false
        /// This view's sizing participant (the attach reply's `participant`); nil until then.
        var participant: String?
        /// The newest size state this view passed on, and one that came before the reply named
        /// the participant (held: without it the view cannot tell whether it owns the grid).
        var sizeGeneration: UInt64?
        var heldSizeState: TerminalSizeState?
        /// This view ended its stream (`cancel-stream`, or closing a connection of its own):
        /// the stream's end is a client detach.
        var canceled = false
    }

    /// A class box, so the reader thread's closure can share the mutex (a `Mutex` cannot be copied).
    final class ControlBox: Sendable {
        let value = Mutex(Control())
    }

    nonisolated let control: ControlBox

    /// The host capability a snapshot attach needs: READY snapshots
    /// (`terminal-snapshot-v1`) followed by their scrollback as `history`
    /// snapshots. A READY alone would drop the scrollback at every attach
    /// and grid change, so a host without it gets a byte replay attach.
    public static let snapshotCapability = DaemonCapability.terminalSnapshotHistory.rawValue

    /// Attaches a view as a stream on the carrier `route` gives for it. The
    /// one attach path of an app view.
    ///
    /// - Parameters:
    ///   - route: Where the view attaches (`DaemonService.terminalAttachRoute`).
    ///   - target: The terminal to attach.
    ///   - size: The view's grid; the attach is a size report.
    ///   - claimGeometry: Claim canonical geometry after the attach.
    ///   - snapshotVersion: The view's GHOSTSNP version (snapshot attach), or nil.
    ///   - localHistory: The view reflows its own history at a resize.
    ///   - images: The view applies Kitty image replays.
    ///   - device: The view's device for shared sizing; nil joins none.
    ///   - hangs: Watches a connection of the view's own (its reader's lines); a shared
    ///     connection's lines are its machine connection's.
    ///   - createIntent: The `create.tab` intent of the new tab this first attach shows; 0 otherwise.
    ///   - inputQueue: The view's serial input queue for a connection of its own (nil: its own queue).
    ///   - clientName: The `set-client-info` name of a connection of the view's own.
    /// - Returns: The attached view.
    /// - Throws: `DaemonError` when the daemon refuses the attach or the
    ///   connection fails; `CancellationError` when the task is cancelled.
    public static func attach(
        route: TerminalAttachRoute,
        target: Target,
        size: CellSize,
        claimGeometry: Bool,
        snapshotVersion: UInt16? = nil,
        localHistory: Bool = false,
        images: Bool = false,
        device: TerminalSizingDevice?,
        hangs: (any HangWatchScoping)? = nil,
        createIntent: UInt64 = 0,
        inputQueue: DispatchQueue? = nil,
        clientName: String = "cmux-next-terminal"
    ) async throws -> TerminalAttachment {
        let carrier = try await route.carrier(TerminalAttachRoute.View(device: device, hangs: hangs, inputQueue: inputQueue,
                                                                       clientName: clientName))
        return try await attach(carrier: carrier, target: target, size: size, claimGeometry: claimGeometry,
                                snapshotVersion: snapshotVersion, localHistory: localHistory, images: images,
                                createIntent: createIntent)
    }

    /// Attaches as a client stream on `carrier`'s connection: one
    /// `attach-surface` round trip (a shared connection declared the sizing
    /// device once, ``DaemonConnection/terminalStreams(for:)``).
    ///
    /// - Parameters:
    ///   - carrier: The connection the stream rides on.
    ///   - target: The terminal to attach.
    ///   - size: The view's grid; the attach is a size report.
    ///   - claimGeometry: Claim canonical geometry after the attach.
    ///   - snapshotVersion: The view's GHOSTSNP version (snapshot attach), or nil.
    ///   - localHistory: The view reflows its own history at a resize.
    ///   - images: The view applies Kitty image replays.
    ///   - createIntent: The `create.tab` intent of the new tab this first attach shows; 0 otherwise.
    /// - Returns: The attached view; its events end with `.closed` at its `stream-end`.
    /// - Throws: `DaemonError` when the daemon refuses the attach or the
    ///   connection fails; `CancellationError` when the task is cancelled.
    public static func attach(
        carrier: TerminalStreamCarrier,
        target: Target,
        size: CellSize,
        claimGeometry: Bool,
        snapshotVersion: UInt16? = nil,
        localHistory: Bool = false,
        images: Bool = false,
        createIntent: UInt64 = 0
    ) async throws -> TerminalAttachment {
        DaemonLaunchTimings.shared.mark("terminal.attach_start")
        NewTerminalTrace.shared.attachEvent(.attachStart, surface: target.surface.rawValue, intent: createIntent)
        let queue = TerminalEventQueue()
        let resolvedSurface = SurfaceBox(target.surface)
        let control = ControlBox()
        let reader = TerminalStreamReader(transport: carrier.transport, queue: queue, control: control,
                                          resolvedSurface: resolvedSurface, ownsConnection: carrier.ownsConnection,
                                          tagsRequests: carrier.tagsStreams)
        guard let stream = carrier.transport.openStream({ reader.receive($0) }) else {
            if carrier.ownsConnection { carrier.transport.close() }
            throw DaemonError.connectionClosed(reason: "the daemon connection closed")
        }
        reader.stream = stream
        let attachment = TerminalAttachment(carrier: carrier, surface: target.surface, stream: stream,
                                            queue: queue, resolvedSurface: resolvedSurface, control: control)
        do {
            try await attachment.openStream(target: target, identity: carrier.identity, size: size,
                                            claimGeometry: claimGeometry, snapshotVersion: snapshotVersion,
                                            localHistory: localHistory, images: images, createIntent: createIntent)
        } catch {
            attachment.abandonStream(after: error)
            throw error
        }
        return attachment
    }

    private init(carrier: TerminalStreamCarrier, surface: SurfaceID, stream: UInt16, queue: TerminalEventQueue,
                 resolvedSurface: SurfaceBox, control: ControlBox) {
        transport = carrier.transport
        ownsConnection = carrier.ownsConnection
        self.stream = stream
        wireStream = carrier.tagsStreams ? stream : nil
        self.resolvedSurface = resolvedSurface
        self.queue = queue
        self.control = control
        events = AsyncStream(unfolding: { await queue.next() }, onCancel: { queue.cancel() })
    }

    private func openStream(target: Target, identity: DaemonIdentity, size: CellSize, claimGeometry: Bool,
                            snapshotVersion: UInt16?, localHistory: Bool, images: Bool, createIntent: UInt64) async throws {
        let useIdentity = identity.supports("attach-identity-v1") && target.terminalResourceID != nil
            && target.generation == identity.generation
        if target.surface == Self.unresolvedSurface, !useIdentity {
            // Only the identity pair can name a terminal with no tab.
            throw DaemonError.missingCapabilities(["attach-identity-v1"])
        }
        let request = AttachSurfaceRequest(
            surface: useIdentity ? nil : target.surface,
            expectedGeneration: useIdentity ? target.generation : nil,
            expectedTerminalID: useIdentity ? target.terminalResourceID : nil,
            size: size,
            snapshotVersion: identity.supports(Self.snapshotCapability) ? snapshotVersion : nil,
            snapshotLocalHistory: localHistory && identity.supports(DaemonCapability.terminalSnapshotLocalHistory.rawValue),
            snapshotImages: images && identity.supports(DaemonCapability.terminalSnapshotImages.rawValue)
        )
        let stream = wireStream
        // The READY or the replay rides before the reply: a longer, still bounded deadline.
        let line = try await transport.request(cmd: AttachSurfaceRequest.commandName, timeout: .seconds(10)) { id in
            try WireCoding.encodeRequest(request, id: id, stream: stream)
        }.line
        let response = try WireCoding.decodeResponse(AttachSurfaceRequest.Response.self, from: line,
                                                     decoder: transport.decoder)
        NewTerminalTrace.shared.attachEvent(.attachReplied, surface: target.surface.rawValue, intent: createIntent)
        acceptAttachReply(response, size: size, hidesInPlace: identity.supports(DaemonCapability.sharedSizing.rawValue))
        if claimGeometry {
            fireAndForget(SetClientSizingRequest(surface: self.surface, enabled: true, exclusive: true))
        }
        DaemonLaunchTimings.shared.mark("terminal.attach_end")
        NewTerminalTrace.shared.attachEvent(.attachEnd, surface: target.surface.rawValue, intent: createIntent)
    }

    /// Ends this view's stream after its attach failed. A connection of the
    /// view's own closes (that ends the stream on the daemon). On a shared
    /// connection the daemon ends a stream it refused (an error reply, then
    /// `stream-end`); a deadline or a cancel gives up with the attach still in
    /// flight: `cancel-stream` runs after it on the stream's lane, so a late
    /// success does not leave a viewer nobody reads; its `stream-end` frees the
    /// id. A closed transport sends nothing more, so the id is forgotten here.
    private nonisolated func abandonStream(after error: any Error) {
        if ownsConnection {
            control.value.withLock { $0.canceled = true }
            transport.close()
            return
        }
        if transport.isClosed {
            transport.forgetStream(stream)
            return
        }
        if case DaemonError.command = error { return }
        control.value.withLock { $0.canceled = true }
        fireAndForget(CancelStreamRequest())
    }

    /// Records the attach reply's lease and size state.
    private nonisolated func acceptAttachReply(_ response: AttachSurfaceRequest.Response, size: CellSize, hidesInPlace: Bool) {
        let queue = queue
        control.value.withLock { control in
            control.lease = response.lease
            control.lastReported = size
            control.hidesInPlace = hidesInPlace
            if let participant = response.participant { control.participant = participant }
            // The reply's state, or a newer one held before the participant was known, now with it.
            // Pushed inside the lock: a newer stream state admitted meanwhile cannot go first.
            guard let participant = control.participant else { return }
            let held = control.heldSizeState
            control.heldSizeState = nil
            let newest = [response.sizeState, held].compactMap { $0 }.max { $0.generation < $1.generation }
            guard let newest, control.sizeGeneration.map({ newest.generation > $0 }) ?? true else { return }
            control.sizeGeneration = newest.generation
            queue.pushWithoutWaiting(.sizeState(newest, participant: participant))
        }
    }

    /// Output bytes waiting for the consumer (diagnostics, tests).
    public nonisolated var bufferedOutputBytes: Int { queue.bufferedOutputBytes }

    /// Passes `state` on with this view's participant, newest generation only. The participant is
    /// the event's `self_participant`, else the attach reply's; with neither the state is held,
    /// and the reply passes on the newer of the two. Pushed under the control lock, so states
    /// reach the view in generation order whichever thread admits them.
    nonisolated static func admitSizeState(_ state: TerminalSizeState, selfParticipant: String?,
                                                   control: ControlBox, queue: TerminalEventQueue) {
        control.value.withLock { control in
            if let selfParticipant { control.participant = selfParticipant }
            guard let participant = control.participant else {
                if control.heldSizeState.map({ state.generation > $0.generation }) ?? true { control.heldSizeState = state }
                return
            }
            guard control.sizeGeneration.map({ state.generation > $0 }) ?? true else { return }
            control.sizeGeneration = state.generation
            queue.pushWithoutWaiting(.sizeState(state, participant: participant))
        }
    }

    // MARK: TerminalByteChannel

    public nonisolated func write(_ data: Data) async {
        enqueueInput(data)
    }

    /// Synchronous, ordered input path for Ghostty's `io_write_cb` thread.
    /// Writes from one thread reach the PTY in call order.
    public nonisolated func enqueueInput(_ data: Data) {
        guard !data.isEmpty else { return }
        let surface = surface
        let logger = logger
        let flow = TypingLatencyProbe.shared.openFlow
        do {
            let stream = wireStream
            try transport.sendNoReply(cmd: SendInputRequest.command, onError: { error in
                logger.error("send failed: \(error.description, privacy: .public)")
            }) { id in
                try WireCoding.encodeRequest(SendInputRequest(surface: surface, bytes: data, flow: flow), id: id, stream: stream)
            }
            if let flow { TypingLatencyProbe.shared.mark(.socketSubmit, flow: flow) }
        } catch {
            logger.debug("input dropped after close: \(String(describing: error), privacy: .public)")
        }
    }

    public func resize(cols: Int, rows: Int, pixelWidth: Int, pixelHeight: Int) async {
        sendResize(CellSize(cols: cols, rows: rows))
    }

    // MARK: Geometry and lifetime

    /// Makes this view the geometry owner: only the owner resizes the PTY.
    public func claimGeometry() {
        claimGeometry(reporting: nil)
    }

    /// Keeps the stream for cached rendering but stops contributing a size
    /// (the view became hidden).
    public func releaseGeometry() {
        sendReleaseGeometry()
    }

    /// Detaches and closes the connection. The terminal keeps running.
    public func detach() {
        detachNow()
    }

    // MARK: Synchronous commands (any thread, sent in call order)

    /// Passive grid report. Skipped when it equals the last report.
    public nonisolated func sendResize(_ size: CellSize) {
        let size = CellSize(cols: max(1, size.cols), rows: max(1, size.rows))
        let lease = control.value.withLock { control -> String?? in
            guard control.lastReported != size else { return .none }
            control.lastReported = size
            return .some(control.lease)
        }
        guard case .some(let lease) = lease else { return }
        if let lease {
            fireAndForget(ResizeAttachedViewRequest(surface: surface, lease: lease, cols: size.cols, rows: size.rows))
        } else {
            fireAndForget(ResizeSurfaceRequest(surface: surface, cols: size.cols, rows: size.rows))
        }
    }

    /// Reports `size` (or the last reported size), then claims canonical
    /// geometry. The daemon accepts a claim only from a view that already
    /// reported a size on this attachment, so the report always goes first.
    public nonisolated func claimGeometry(reporting size: CellSize?) {
        let (lease, report) = control.value.withLock { control -> (String?, CellSize?) in
            if let size { control.lastReported = CellSize(cols: max(1, size.cols), rows: max(1, size.rows)) }
            return (control.lease, control.lastReported)
        }
        if let report, let lease {
            fireAndForget(ResizeAttachedViewRequest(surface: surface, lease: lease, cols: report.cols, rows: report.rows))
        }
        fireAndForget(SetClientSizingRequest(surface: surface, enabled: true, exclusive: true))
    }

    /// Take over ("Sized for" chip): under a policy that picks by size (`setLatest`) the terminal's
    /// policy becomes last active first, then this view reports `size` and claims, which the
    /// daemon counts as activity. Sent in this order on one connection.
    public nonisolated func takeOver(reporting size: CellSize, setLatest: Bool) {
        if setLatest, control.value.withLock({ $0.hidesInPlace }) {
            fireAndForget(SetSizePolicyRequest(surface: surface, policy: .init(mode: "latest")))
        }
        claimGeometry(reporting: size)
    }

    /// Asks the host for a fresh READY + history on this attach (the view's
    /// local history did not match the host's check).
    public nonisolated func requestSnapshot(reason: SnapshotRequestReason) {
        fireAndForget(SnapshotRequestRequest(surface: surface, reason: reason))
    }

    public nonisolated func sendReleaseGeometry() {
        let lease = control.value.withLock { control -> String? in
            control.lastReported = nil
            return control.lease
        }
        guard let lease else { return }
        fireAndForget(ReleaseAttachedViewSizeRequest(surface: surface, lease: lease))
    }

    /// The view hid (a parked tab or workspace) or shows again. Hidden, it keeps its lease, stream,
    /// viewport and claim and only stops counting toward the grid, so hiding never resizes the PTY
    /// when it is the only view, other views get the grid by their own size, and showing it again
    /// at the same size needs no report or claim. A daemon without `shared-sizing-v1` gets the old
    /// release, and a claim of the last reported size on show.
    public nonisolated func sendHidden(_ hidden: Bool) {
        let (lease, inPlace) = control.value.withLock { ($0.lease, $0.hidesInPlace) }
        guard let lease else { return }
        if inPlace {
            fireAndForget(SetSizeCountsRequest(surface: surface, lease: lease, counts: hidden ? false : nil))
        } else if hidden {
            fireAndForget(ReleaseAttachedViewSizeRequest(surface: surface, lease: lease))
        } else {
            claimGeometry(reporting: nil)
        }
    }

    /// Idempotent: the first call detaches. The stream ends: `cancel-stream`
    /// on a shared connection (which stays), or a connection of its own closes.
    public nonisolated func detachNow() {
        let first = control.value.withLock { control -> Bool in
            guard !control.detached else { return false }
            control.detached = true
            control.canceled = true
            return true
        }
        guard first else { return }
        if ownsConnection {
            transport.close()
        } else {
            fireAndForget(CancelStreamRequest())
        }
    }

    private nonisolated func fireAndForget<R: DaemonRequest>(_ request: R) {
        let logger = logger
        let stream = wireStream
        do {
            try transport.sendNoReply(cmd: R.command, onError: { error in
                logger.error("\(R.command, privacy: .public) failed: \(error.description, privacy: .public)")
            }) { id in
                try WireCoding.encodeRequest(request, id: id, stream: stream)
            }
        } catch {
            logger.debug("\(R.command, privacy: .public) dropped after close")
        }
    }
}
