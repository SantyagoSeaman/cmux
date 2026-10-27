import Foundation

/// The agent session verbs (`agent-session-*`) on this connection: ``AgentSessionAttachClient`` sends
/// them here, and the attachments they open stream on this connection (``agentSessions``) instead of
/// a socket of their own (p05-ipc-102).
extension DaemonConnection {
    /// An attachment endpoint ``sendAgentSession(_:timeout:opening:fields:)`` registers with its request.
    struct AgentSessionOpening: Sendable {
        let surface: UInt64
        let sink: AgentSessionStreamRoute.Sink
    }

    /// The identity of the ready connection, waiting up to `limit` for one while it reconnects.
    func agentSessionIdentity(within limit: Duration) async throws -> DaemonIdentity {
        if ready == nil, !(await untilReady(after: nil, within: limit)) { throw DaemonError.notConnected }
        guard let ready else { throw DaemonError.notConnected }
        return ready.identity
    }

    /// Sends one agent session verb whose `fields` are its JSON object without `id`, and answers the
    /// raw `ok:true` reply. With `opening`, the endpoint is registered on the transport that carries
    /// the request, before the line goes out, so no push of the attachment it opens is missed.
    func sendAgentSession(_ cmd: DaemonCommandName, timeout: Duration, opening: AgentSessionOpening? = nil,
                          fields: Data) async throws -> (response: LineTransport.Response, token: AgentSessionStreamRoute.Token?) {
        await waitOutHandoff()
        guard let ready else { throw DaemonError.notConnected }
        let token = opening.map { agentSessions.open(surface: $0.surface, serial: ready.serial, sink: $0.sink) }
        requestStarted()
        defer { requestEnded() }
        do {
            let response = try await ready.transport.request(cmd: cmd, timeout: timeout) { id in
                Data(#"{"id":\#(id),"#.utf8) + fields.dropFirst()
            }
            return (response, token)
        } catch {
            // A refused or lost attach leaves no viewer; a timed-out one may attach late, so detach it.
            if let token { await closeAgentSessionStream(token, timeout: timeout) }
            throw error
        }
    }

    /// Closes `token`; when it was the last endpoint of its surface on a connection still up, the
    /// daemon's attachment is detached (`agent-session-detach`). A lost connection took the
    /// attachment with it, and a newer connection's attachments are never touched.
    func closeAgentSessionStream(_ token: AgentSessionStreamRoute.Token, timeout: Duration) async {
        guard agentSessions.close(token), let ready, ready.serial == token.serial else { return }
        _ = try? await ready.transport.request(cmd: "agent-session-detach", timeout: timeout) { id in
            Data(#"{"id":\#(id),"cmd":"agent-session-detach","surface":\#(token.surface)}"#.utf8)
        }
    }
}
