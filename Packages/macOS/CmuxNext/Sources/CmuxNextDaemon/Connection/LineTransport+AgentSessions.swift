import Foundation

/// Takes one agent attachment's messages from a connection's reader thread (``LineTransport/openAgentSession(surface:_:)``).
/// `receive` must not block: every stream and the control plane share the reader.
protocol AgentSessionReceiver: AnyObject, Sendable {
    func receive(_ message: LineTransport.AgentSessionMessage)
}

extension LineTransport {
    /// One message of an agent session attachment on this connection (`agent-session-attach-v1`).
    enum AgentSessionMessage: Sendable {
        /// `agent-session-record`, `-permission`, `-changed` or `-closed`, with the line's JSON object.
        /// `agent-session-closed` is the attachment's last one: the route forgets it before delivering it.
        case event(name: String, line: Data)
        /// The connection ended; no message follows.
        case closed(TransportCloseReason)
    }

    /// The agent session events of the daemon (cmux-tui/spec/commands.md "Agent session attach"). Each names its
    /// tab's `surface` and belongs to the one attachment of that surface on this connection, never to stream 0.
    static let agentSessionEvents: Set<String> = [
        "agent-session-record", "agent-session-permission", "agent-session-changed", "agent-session-closed",
    ]

    /// The agent attachments of one connection, by surface: the daemon keeps one per (connection, surface),
    /// so a surface has at most one receiver here. As a mojo endpoint router keeps one endpoint per interface
    /// id of a pipe (mojo/public/cpp/bindings/lib/multiplex_router.cc).
    struct AgentSessionRoutes {
        private var receivers: [UInt64: any AgentSessionReceiver] = [:]

        var count: Int { receivers.count }

        /// Registers `receiver` for `surface`; false when another one holds it.
        mutating func open(surface: UInt64, _ receiver: any AgentSessionReceiver) -> Bool {
            guard receivers[surface] == nil else { return false }
            receivers[surface] = receiver
            return true
        }

        /// Forgets `surface` when `receiver` holds it; true when it did.
        mutating func close(surface: UInt64, _ receiver: any AgentSessionReceiver) -> Bool {
            guard let held = receivers[surface], held === receiver else { return false }
            receivers[surface] = nil
            return true
        }

        func holds(surface: UInt64, _ receiver: any AgentSessionReceiver) -> Bool {
            receivers[surface].map { $0 === receiver } ?? false
        }

        /// The receiver of `surface`; taken out when `ending` (its `agent-session-closed`).
        mutating func receiver(_ surface: UInt64, ending: Bool) -> (any AgentSessionReceiver)? {
            ending ? receivers.removeValue(forKey: surface) : receivers[surface]
        }

        /// Takes every receiver out (the connection ended).
        mutating func removeAll() -> [any AgentSessionReceiver] {
            defer { receivers.removeAll() }
            return Array(receivers.values)
        }
    }

    /// Registers `receiver` as the attachment of `surface` on this connection, before the request that
    /// attaches it, so the records that follow the attach reply are routed. False when another receiver
    /// holds the surface or the transport closed.
    func openAgentSession(surface: UInt64, _ receiver: any AgentSessionReceiver) -> Bool {
        state.withLock { state in
            guard state.closed == nil else { return false }
            return state.agentSessions.open(surface: surface, receiver)
        }
    }

    /// Forgets `receiver`'s attachment of `surface`; true when it still held it (the daemon may still
    /// send for it until it hears `agent-session-detach`; those events are dropped here).
    @discardableResult
    func closeAgentSession(surface: UInt64, _ receiver: any AgentSessionReceiver) -> Bool {
        state.withLock { $0.agentSessions.close(surface: surface, receiver) }
    }

    /// `receiver` still holds `surface` (the daemon has not ended the attachment).
    func holdsAgentSession(surface: UInt64, _ receiver: any AgentSessionReceiver) -> Bool {
        state.withLock { $0.agentSessions.holds(surface: surface, receiver) }
    }

    /// Open agent attachments on this connection (diagnostics, tests).
    var agentSessionCount: Int { state.withLock { $0.agentSessions.count } }

    /// Routes one agent session event to the attachment of its surface, on the reader thread. It never
    /// counts as a stream-0 event: an event for a surface nothing holds (one detached a moment ago) is dropped.
    func routeAgentSession(name: String, line: Data, decoder: WireDecoder) {
        let surface = (try? decoder.decode(SurfaceField.self, from: line))?.surface
        let receiver = surface.flatMap { surface in
            state.withLock { $0.agentSessions.receiver(surface, ending: name == "agent-session-closed") }
        }
        guard let receiver else {
            recorder?.lineRead(.ignored)
            return
        }
        recorder?.lineRead(.event)
        receiver.receive(.event(name: name, line: line))
    }

    /// Ends every agent attachment of the connection with `reason` (the transport's close); once.
    func endAgentSessions(_ reason: TransportCloseReason) {
        let ended = state.withLock { $0.agentSessions.removeAll() }
        for receiver in ended { receiver.receive(.closed(reason)) }
    }

    /// The one field an agent session event is routed by.
    private struct SurfaceField: Decodable {
        var surface: UInt64?
    }
}
