import Foundation

extension DaemonConnection {
    /// The carrier of a terminal view's stream: every view is a stream. With
    /// ``DaemonConnectionConfiguration/terminalStreams`` (`daemonTerminalStreams`)
    /// and a daemon that serves `attach-streams-v1` it is this ready connection,
    /// shared by every view with no connection or handshake of its own;
    /// otherwise a connection of the view's own to this connection's daemon
    /// that carries only that stream (an older daemon's untagged events are the
    /// stream's, ``TerminalStreamCarrier/tagsStreams``).
    ///
    /// The first shared call of each connect declares the views' sizing
    /// identity once for the connection (`set-client-info` with
    /// `shared-sizing-v1` and `device`; the daemon merges it into the
    /// handshake's): the declaration every per-view connection used to send
    /// for itself.
    ///
    /// - Parameter view: The view's device, and for a connection of its own
    ///   its hang watch, input queue and client name.
    /// - Returns: The carrier of the view's stream.
    /// - Throws: `DaemonError.notConnected` while the connection is down, so a
    ///   view waits for it (its attach retries) instead of opening a connection
    ///   of its own meanwhile; `DaemonError` when a connection of its own fails.
    public func terminalStreams(for view: TerminalAttachRoute.View) async throws -> TerminalStreamCarrier {
        guard let ready, let endpoint else { throw DaemonError.notConnected }
        let streams = DaemonCapability.attachStreams.rawValue
        guard configuration.terminalStreams, ready.identity.supports(streams),
              configuration.advertisedCapabilities.contains(streams) else {
            return try await TerminalStreamCarrier.dedicated(
                endpoint: endpoint, capabilities: configuration.advertisedCapabilities, view: view)
        }
        if let device = view.device {
            try await declareStreamSizing(device, on: ready)
        }
        let frames = ready.identity.supports(DaemonCapability.attachFrames.rawValue)
            && configuration.advertisedCapabilities.contains(DaemonCapability.attachFrames.rawValue)
        return TerminalStreamCarrier(transport: ready.transport, identity: ready.identity, frames: frames,
                                     ownsConnection: false, tagsStreams: true)
    }

    /// One `set-client-info` per connect, shared by every caller of this
    /// connect (a failed one is tried again by the next caller).
    private func declareStreamSizing(_ device: TerminalSizingDevice, on ready: DaemonReadyConnection) async throws {
        if let declared = streamSizingDeclaration, declared.serial == ready.serial {
            return try await declared.task.value
        }
        let transport = ready.transport
        // task-owner: the declaration of this connect; callers await it, a new connect replaces it
        let task = Task {
            _ = try await Self.perform(SetClientInfoRequest(
                name: nil, kind: nil, capabilities: [DaemonCapability.sharedSizing.rawValue],
                deviceKind: device.kind, deviceName: device.name), on: transport)
        }
        streamSizingDeclaration = (ready.serial, task)
        do {
            try await task.value
        } catch {
            if streamSizingDeclaration?.serial == ready.serial { streamSizingDeclaration = nil }
            throw error
        }
    }
}

extension TerminalStreamCarrier {
    /// A connection of one view's own that carries only its stream: one
    /// round trip of `identify` and `set-client-info` (the view's device joins
    /// shared sizing), then the view attaches as a stream on it. It closes when
    /// that stream ends (``ownsConnection``). A daemon without
    /// `attach-streams-v1` tags nothing: every event on the connection is the
    /// view's, and its requests name no stream (``tagsStreams``).
    ///
    /// - Parameters:
    ///   - endpoint: The daemon's socket (and bridge, for a paired machine).
    ///   - capabilities: What the connection advertises: its machine connection's.
    ///   - view: The view's device, hang watch, input queue and client name.
    /// - Returns: The carrier, owning its connection.
    /// - Throws: `DaemonError` when the connection or its handshake fails.
    static func dedicated(endpoint: DaemonEndpoint, capabilities: [String],
                          view: TerminalAttachRoute.View) async throws -> TerminalStreamCarrier {
        let transport = try await LineTransport.open(path: endpoint.socketPath, bridge: endpoint.bridge, role: .terminalAttach,
                                                     stats: endpoint.wireStats, hangs: view.hangs,
                                                     writerTarget: view.inputQueue)
        // A tagged message goes to its stream; an untagged event is the one view's (an older daemon). The view's
        // stream holds the transport until the stream ends, so the handler does not.
        transport.start(onEvent: { [weak transport] name, line, _ in
            transport?.deliverToOpenStreams(.event(name: name, line: line))
        }, onClose: { _ in })
        do {
            var advertised = capabilities
            if view.device != nil { advertised.append(DaemonCapability.sharedSizing.rawValue) }
            let replies = await transport.pipeline([
                PipelinedLine(IdentifyRequest()),
                PipelinedLine(SetClientInfoRequest(name: view.clientName, kind: "frontend", capabilities: advertised,
                                                   deviceKind: view.device?.kind, deviceName: view.device?.name)),
            ], timeout: DaemonConnection.defaultRequestTimeout)
            let identity = try WireCoding.decodeResponse(IdentifyRequest.Response.self, from: replies[0].get())
            _ = try replies[1].get()
            let streams = DaemonCapability.attachStreams.rawValue
            let tagsStreams = identity.supports(streams) && capabilities.contains(streams)
            let frames = tagsStreams && identity.supports(DaemonCapability.attachFrames.rawValue)
                && capabilities.contains(DaemonCapability.attachFrames.rawValue)
            return TerminalStreamCarrier(transport: transport, identity: identity, frames: frames, ownsConnection: true,
                                         tagsStreams: tagsStreams)
        } catch {
            transport.close()
            throw error
        }
    }
}
