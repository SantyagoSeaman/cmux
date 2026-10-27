public import CmuxNextDaemon

extension DaemonService {
    /// The carrier of this machine's agent attachments: its connection, shared by every agent pane of the
    /// machine (``AgentSessionAttachClient``'s `shared`). While the connection reconnects it waits up to `limit`
    /// for the next one, so a pane never opens a connection of its own for a reconnect. Throws when an
    /// administrator turned the machine off, or no connection is ready in time.
    public func agentSessionCarrier(within limit: Duration) async throws -> AgentSessionCarrier {
        if policyBlock.isBlocked { throw DaemonError.endpointBlocked("turned off by your organization") }
        if connection == nil, startup == .connecting, isStarting { await firstConnection() }
        guard let connection else { throw DaemonError.notConnected }
        return try await connection.agentSessionCarrier(within: limit)
    }

    /// ``agentSessionCarrier(within:)`` as the provider an ``AgentSessionAttachClient`` asks on each connect.
    public var agentSessionCarrierProvider: AgentSessionAttachClient.CarrierProvider {
        { [weak self] limit in
            guard let self else { throw DaemonError.notConnected }
            return try await self.agentSessionCarrier(within: limit)
        }
    }
}
