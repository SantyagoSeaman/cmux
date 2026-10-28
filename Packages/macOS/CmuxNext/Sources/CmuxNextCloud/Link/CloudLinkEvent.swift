import Foundation

/// The JSON lines cmux-tui helpers print on stdout.
package enum CloudLinkEvent: Equatable, Sendable {
    /// `wg hub`: `{"event":"hub-ready","socket":…,"routes":[…]}`.
    case hubReady(socket: String)
    /// `remote connect --headless --json`: the first
    /// `{"event":"connection-snapshot","local_socket":…,"local_socket_features":[…]}` names the local v12
    /// socket the app connects to like a local daemon, and what it carries besides JSON lines.
    case connected(LinkLocalSocket)
    case other

    package static func parse(_ line: String) -> CloudLinkEvent {
        guard let data = line.data(using: .utf8),
              let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let event = object["event"] as? String else { return .other }
        switch event {
        case "hub-ready":
            guard let socket = object["socket"] as? String, !socket.isEmpty else { return .other }
            return .hubReady(socket: socket)
        case "connection-snapshot":
            guard let socket = object["local_socket"] as? String, !socket.isEmpty else { return .other }
            return .connected(LinkLocalSocket(path: socket, carriesFrames: carriesFrames(object)))
        default:
            return .other
        }
    }

    /// A later `connection-snapshot` of a running link: whether its
    /// `connection.state` is `connected` (false while it redials, after it
    /// gave up or closed); nil for any other line.
    package static func linkConnected(_ line: String) -> Bool? {
        guard let data = line.data(using: .utf8),
              let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              object["event"] as? String == "connection-snapshot",
              let state = (object["connection"] as? [String: Any])?["state"] as? String else { return nil }
        return state == "connected"
    }

    /// A later `connection-snapshot` of a running link: whether its socket now carries frames (the daemon behind
    /// it may have been replaced); nil for any other line.
    package static func localSocketCarriesFrames(_ line: String) -> Bool? {
        guard let data = line.data(using: .utf8),
              let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              object["event"] as? String == "connection-snapshot" else { return nil }
        return carriesFrames(object)
    }

    /// Whether a snapshot's `local_socket_features` lists `bridge-frames-v1` (cmux-tui/spec/remote-daemon.md,
    /// "Services"); a link that predates the field lists nothing.
    private static func carriesFrames(_ snapshot: [String: Any]) -> Bool {
        (snapshot["local_socket_features"] as? [String] ?? []).contains(LinkLocalSocket.framesFeature)
    }
}

/// A link's local socket (`cmux-tui remote connect`), and whether it passes cmux-ipc frames: the daemon's
/// `mux-control` open marker listed `bridge-frames-v1`. Without it the socket cuts JSON lines only.
public struct LinkLocalSocket: Equatable, Sendable {
    package static let framesFeature = "bridge-frames-v1"

    public let path: String
    public let carriesFrames: Bool

    public init(path: String, carriesFrames: Bool) {
        self.path = path
        self.carriesFrames = carriesFrames
    }
}
