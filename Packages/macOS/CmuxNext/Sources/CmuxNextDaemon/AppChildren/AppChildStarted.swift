import Foundation

/// A child the daemon started for this app (`app-child-start`), or one `app-child-list` found again
/// after the app relaunched. `secret` is a lifeline child's hello token: keep it in memory only.
public struct AppChildStarted: Decodable, Sendable, Equatable {
    public var child: String
    public var pid: Int32
    public var secret: String?
    /// The first output line that contained the start's `ready` text.
    public var readyLine: String?

    enum CodingKeys: String, CodingKey {
        case child, pid, secret
        case readyLine = "ready_line"
    }

    public init(child: String, pid: Int32, secret: String? = nil, readyLine: String? = nil) {
        self.child = child
        self.pid = pid
        self.secret = secret
        self.readyLine = readyLine
    }
}

/// One `app-child-list` row.
public struct AppChildRow: Decodable, Sendable, Equatable {
    public var child: String
    public var pid: Int32
    public var name: String
    public var owner: String
    public var running: Bool
    public var secret: String?
    public var readyLine: String?

    enum CodingKeys: String, CodingKey {
        case child, pid, name, owner, running, secret
        case readyLine = "ready_line"
    }

    public init(child: String, pid: Int32, name: String, owner: String, running: Bool, secret: String? = nil, readyLine: String? = nil) {
        self.child = child
        self.pid = pid
        self.name = name
        self.owner = owner
        self.running = running
        self.secret = secret
        self.readyLine = readyLine
    }

    /// The row as the start answered it.
    public var started: AppChildStarted {
        AppChildStarted(child: child, pid: pid, secret: secret, readyLine: readyLine)
    }
}
