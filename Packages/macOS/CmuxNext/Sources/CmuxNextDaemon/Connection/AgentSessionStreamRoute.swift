import CmuxNextCompat
import Foundation

/// The agent session attachments a ``DaemonConnection`` carries (`agent-session-attach-v1`,
/// cmux-tui/spec/commands.md "Agent session attach"). The connection's reader hands each attachment
/// push (`agent-session-record`, `-permission`, `-changed`, `-closed`) to the endpoints of its
/// `surface` and nowhere else, so a chat transcript never enters the store's inbox or the main
/// thread's event drain. A connection that ends ends every endpoint it carried with
/// `agent-session-closed` (`connection_lost`), as the daemon ends their attachments.
///
/// Chrome runs many associated interfaces over one message pipe, and its router hands each message to
/// the endpoint its header names (mojo/public/cpp/bindings/README.md, "Associated Interfaces";
/// mojo/public/cpp/bindings/lib/multiplex_router.cc, `MultiplexRouter::Accept`); a pipe error ends
/// every endpoint (`MultiplexRouter::OnPipeConnectionError`). Here the surface is the endpoint id, and
/// the daemon keeps one attachment per connection and surface, so the route counts the endpoints of a
/// surface and only the last one to close detaches it.
final class AgentSessionStreamRoute: Sendable {
    /// Takes one push: its event name and the whole line. Runs on the connection's reader thread (or the
    /// connection's executor for the `connection_lost` close), never under the route's lock.
    typealias Sink = @Sendable (_ event: String, _ line: Data) -> Void

    /// One registered endpoint: the surface it shows and the connection serial it lives on.
    struct Token: Sendable, Hashable {
        let id: UInt64
        let surface: UInt64
        let serial: UInt64
    }

    private struct Endpoint {
        let token: Token
        let sink: Sink
    }

    private struct State {
        var endpoints: [UInt64: [Endpoint]] = [:]
        var lastID: UInt64 = 0
    }

    private let state = Mutex(State())

    /// The close a connection loss hands every endpoint it ends.
    static let connectionLost = Data(#"{"event":"agent-session-closed","reason":"connection_lost"}"#.utf8)

    init() {}

    /// True for a line of this family: the reader hands it to ``deliver(name:line:serial:decoder:)``.
    static func carries(_ name: String) -> Bool { name.hasPrefix("agent-session-") }

    /// The endpoints the route holds (tests).
    var count: Int { state.withLock { $0.endpoints.values.reduce(0) { $0 + $1.count } } }

    /// Registers `sink` for the pushes of `surface` on connection `serial`. Register before the
    /// `agent-session-attach` that opens the stream: the daemon's first record can follow its reply
    /// on the reader before the caller resumes.
    func open(surface: UInt64, serial: UInt64, sink: @escaping Sink) -> Token {
        state.withLock { state in
            state.lastID += 1
            let token = Token(id: state.lastID, surface: surface, serial: serial)
            state.endpoints[surface, default: []].append(Endpoint(token: token, sink: sink))
            return token
        }
    }

    /// True while `token` is registered (its connection has not ended and nobody closed it).
    func isOpen(_ token: Token) -> Bool {
        state.withLock { $0.endpoints[token.surface]?.contains { $0.token == token } ?? false }
    }

    /// Removes `token`. True when it was the last endpoint of its surface on its serial: the daemon's
    /// attachment then has no viewer left and the caller detaches it.
    func close(_ token: Token) -> Bool {
        state.withLock { state in
            guard var endpoints = state.endpoints[token.surface],
                  let index = endpoints.firstIndex(where: { $0.token == token }) else { return false }
            endpoints.remove(at: index)
            let last = !endpoints.contains { $0.token.serial == token.serial }
            state.endpoints[token.surface] = endpoints.isEmpty ? nil : endpoints
            return last
        }
    }

    /// Hands one push of connection `serial` to the endpoints of its surface; a push no endpoint
    /// shows is dropped. The line is parsed once here for its `surface`; the sink reads its body.
    func deliver(name: String, line: Data, serial: UInt64, decoder: WireDecoder) {
        guard let surface = (try? decoder.decode(SurfaceField.self, from: line))?.surface else { return }
        let sinks = state.withLock { state in
            (state.endpoints[surface] ?? []).filter { $0.token.serial == serial }.map(\.sink)
        }
        for sink in sinks { sink(name, line) }
    }

    /// Connection `serial` ended: each of its endpoints gets `agent-session-closed`
    /// (`connection_lost`) once and is removed.
    func end(serial: UInt64) {
        let ended = state.withLock { state -> [Sink] in
            var ended: [Sink] = []
            for (surface, endpoints) in state.endpoints {
                ended += endpoints.filter { $0.token.serial == serial }.map(\.sink)
                let kept = endpoints.filter { $0.token.serial != serial }
                state.endpoints[surface] = kept.isEmpty ? nil : kept
            }
            return ended
        }
        for sink in ended { sink("agent-session-closed", Self.connectionLost) }
    }

    private struct SurfaceField: Decodable {
        let surface: UInt64?
    }
}
