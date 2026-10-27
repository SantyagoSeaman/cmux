import Foundation

/// One agent attachment as a stream on a daemon connection: the connection's reader hands the events of its
/// surface here (``LineTransport/openAgentSession(surface:_:)``), and an ``AgentSessionEventQueue`` carries them
/// to the page. Many attachments share one connection and its one reader thread; none has a socket of its own.
final class AgentSessionStream: AgentSessionReceiver {
    static let closedEvent = "agent-session-closed"

    let surface: UInt64
    let transport: LineTransport
    private let queue: AgentSessionEventQueue

    /// Registers the stream for `surface` on `transport`; nil when another stream holds that surface there
    /// (the daemon keeps one attachment per connection and surface) or the transport closed.
    ///
    /// - Parameters:
    ///   - limits: How much may wait for `deliver` before the stream ends with `overflow`.
    ///   - deliver: Takes each event in order, off the reader thread; `agent-session-closed` is the last.
    static func open(surface: UInt64, on transport: LineTransport, limits: AgentSessionEventQueue.Limits = .init(),
                     deliver: @escaping @Sendable (_ event: String, _ line: Data) -> Void) -> AgentSessionStream? {
        let queue = AgentSessionEventQueue(limits: limits, deliver: deliver)
        let stream = AgentSessionStream(surface: surface, transport: transport, queue: queue)
        guard transport.openAgentSession(surface: surface, stream) else {
            queue.cancel()
            return nil
        }
        return stream
    }

    private init(surface: UInt64, transport: LineTransport, queue: AgentSessionEventQueue) {
        self.surface = surface
        self.transport = transport
        self.queue = queue
    }

    /// The daemon has not ended the attachment and this side did not end it.
    var isOpen: Bool { transport.holdsAgentSession(surface: surface, self) }

    func receive(_ message: LineTransport.AgentSessionMessage) {
        switch message {
        case .event(let name, let line):
            switch queue.push(name, line) {
            case .queued:
                // The route forgot the attachment before this, its last event.
                if name == Self.closedEvent { queue.end(closing: nil) }
            case .ended:
                break
            case .overflowed:
                // The daemon's attachment ends too, so it stops sending; the page attaches again after the
                // newest record it got.
                try? transport.sendNoReply(cmd: Self.detachCommand.wire) { try detachLine($0) }
                queue.end(closing: Self.closedLine(surface, reason: "overflow"))
            }
        case .closed:
            queue.end(closing: Self.closedLine(surface, reason: "connection_lost"))
        }
    }

    /// Ends the stream from this side with no wait: nothing more reaches the page, and when it still held its
    /// surface the daemon detaches it (`sendsDetach`; a connection that closes next needs none).
    func end(sendsDetach: Bool) {
        queue.cancel()
        if sendsDetach {
            try? transport.sendNoReply(cmd: Self.detachCommand.wire) { try detachLine($0) }
        } else {
            transport.closeAgentSession(surface: surface, self)
        }
    }

    /// Ends the stream from this side and waits for the daemon's answer to its `agent-session-detach`; sends
    /// none when the daemon already ended the attachment.
    func detach(timeout: Duration) async {
        queue.cancel()
        _ = try? await transport.request(cmd: Self.detachCommand, timeout: timeout) { try detachLine($0) }
    }

    /// The `agent-session-detach` request `id`, made while the line is written: the stream lets go of its surface
    /// in the same step, so no later attachment of that surface on this connection is written before it. Throws
    /// when the stream no longer holds the surface (the daemon ended it): nothing is sent then, as a detach would
    /// end whichever attachment holds the surface next.
    private func detachLine(_ id: UInt64) throws -> Data {
        guard transport.closeAgentSession(surface: surface, self) else { throw DaemonError.connectionClosed(reason: "agent attachment ended") }
        return Data(#"{"id":\#(id),"cmd":"agent-session-detach","surface":\#(surface)}"#.utf8)
    }

    private static let detachCommand: DaemonCommandName = "agent-session-detach"

    /// An `agent-session-closed` line this side makes (the connection ended, or the page fell behind).
    static func closedLine(_ surface: UInt64, reason: String) -> Data {
        Data(#"{"event":"agent-session-closed","surface":\#(surface),"reason":"\#(reason)"}"#.utf8)
    }
}
