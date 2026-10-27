/// A refused attach verb: the daemon's `error_code` (`agent_session.*`) and text.
public struct AgentSessionAttachError: Error, Sendable, Equatable {
    public var code: String
    public var message: String
    /// A typed refusal's facts (the response's `data`), when it has them.
    public var details: JSONValue?

    public init(code: String, message: String, details: JSONValue? = nil) {
        self.code = code
        self.message = message
        self.details = details
    }

    /// `agent_session.needs_keyed_start`: whether that machine's daemon says a keyed restart of
    /// its acpmux loses nothing (`safe_to_restart`); nil for any other refusal.
    public var keyedRestartSafe: Bool? {
        guard code == Self.needsKeyedStart else { return nil }
        return details?["safe_to_restart"]?.boolValue ?? false
    }

    public static let needsKeyedStart = "agent_session.needs_keyed_start"
}
