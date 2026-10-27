public import CmuxNextDaemon

extension DaemonService {
    /// The carrier of this machine's agent attachments: its connection, shared by every agent pane of the
    /// machine (``AgentSessionAttachClient``'s `shared`). Nil while an administrator turned the machine off
    /// or it is not connected; the client then opens a connection of its own.
    public func agentSessionCarrier() async -> AgentSessionCarrier? {
        guard !policyBlock.isBlocked, let connection else { return nil }
        return connection.agentSessionCarrier()
    }

    /// ``agentSessionCarrier()`` as the provider an ``AgentSessionAttachClient`` asks on each connect.
    public var agentSessionCarrierProvider: AgentSessionAttachClient.CarrierProvider {
        { [weak self] in await self?.agentSessionCarrier() }
    }
}
