import Foundation

/// A daemon connection that carries agent attachments as streams (``AgentSessionStream``): the machine's
/// ready connection, shared by every agent pane of that machine with no socket, reader thread or handshake of
/// their own (``DaemonConnection/agentSessionCarrier()``), or a connection of one client's own
/// (``ownsConnection``) where the shared one cannot carry it.
///
/// A shared carrier is valid for that connection's current transport only: when it ends, every attachment on
/// it ends with `connection_lost` and the pages attach again on the next one.
public struct AgentSessionCarrier: Sendable {
    let transport: LineTransport
    let identity: DaemonIdentity
    /// The connection is one client's own: it closes with that client.
    let ownsConnection: Bool
}

extension DaemonConnection {
    /// This connection as the carrier of agent attachments, or nil while it is not ready. Reads the ready
    /// connection's snapshot with no actor hop.
    public nonisolated func agentSessionCarrier() -> AgentSessionCarrier? {
        guard let ready = admittedConnection, !ready.transport.isClosed else { return nil }
        return AgentSessionCarrier(transport: ready.transport, identity: ready.identity, ownsConnection: false)
    }
}
