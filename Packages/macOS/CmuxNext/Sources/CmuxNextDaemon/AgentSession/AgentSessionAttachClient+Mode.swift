extension AgentSessionAttachClient {
    /// How the daemon attachment is used. Observation has its own capability and command so a
    /// missing observer feature can never silently fall back to a writable attach.
    public enum Mode: Sendable, Equatable {
        case attach
        case observe

        var capability: String {
            switch self {
            case .attach: return DaemonCapability.agentSessionAttach.rawValue
            case .observe: return DaemonCapability.agentSessionObserve.rawValue
            }
        }

        var command: DaemonCommandName {
            switch self {
            case .attach: return "agent-session-attach"
            case .observe: return "agent-session-observe"
            }
        }

        var clientInfoKind: String { command.wire }
        var clientInfoName: String {
            switch self {
            case .attach: return "cmux-next agent attach"
            case .observe: return "cmux-next agent observer"
            }
        }

        var unsupportedMessage: String {
            let feature = self == .observe ? "agent session observation" : "agent session attach"
            return "the daemon does not serve \(feature)"
        }
    }
}
