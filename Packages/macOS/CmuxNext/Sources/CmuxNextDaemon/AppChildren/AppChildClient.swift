public import Foundation

/// The app's side of `app-children-v1` (p01-process-239): its long-lived children run under the local
/// daemon, which keeps them when the app crashes and ends them when the app quits or does not come back
/// within its grace (cmux-tui-core/src/server/app_children.rs, the lifeline rule). Only the verified app
/// connection may use it; every call throws ``DaemonError`` as the daemon answered.
///
/// ```swift
/// let children = AppChildClient(connection: daemon)
/// let started = try await children.start(AppChildSpec(name: "serve-web", owner: "vscode", executable: url))
/// let chunk = try await children.read(started.child, after: 0, wait: .seconds(10))
/// ```
public struct AppChildClient: Sendable {
    /// The extra time a request gets past the daemon's own wait (the reply's trip).
    static let replyMargin: Duration = .seconds(5)

    private let connection: DaemonConnection

    public init(connection: DaemonConnection) {
        self.connection = connection
    }

    /// Whether the connected daemon serves app children.
    public var isServed: Bool {
        get async { await connection.identity?.supports(DaemonCapability.appChildren.rawValue) == true }
    }

    /// Starts `spec`; answers once it runs, or once it printed its ``AppChildSpec/ready`` line.
    public func start(_ spec: AppChildSpec) async throws -> AppChildStarted {
        let request = AppChildRequests.Start(
            name: spec.name, owner: spec.owner, executable: spec.executable.path, args: spec.arguments,
            env: spec.environment, cwd: spec.workingDirectory?.path, stdin: spec.lifeline ? "lifeline" : nil,
            secret: spec.secret, ready: spec.ready, readyTimeoutMS: spec.readyTimeout.map(Self.milliseconds))
        let wait = spec.ready == nil ? Duration.zero : (spec.readyTimeout ?? .seconds(60))
        return try await connection.request(request, timeout: wait + Self.replyMargin)
    }

    /// Output after `after`, waiting up to `wait` (the daemon caps it at 10 s) when there is none yet.
    public func read(_ child: String, after: UInt64, wait: Duration) async throws -> AppChildChunk {
        let request = AppChildRequests.Read(child: child, after: after, waitMS: Self.milliseconds(wait), untilExit: false)
        return try await connection.request(request, timeout: wait + Self.replyMargin)
    }

    /// Returns once `child` ended and its output closed: an exit event, not a poll (no deadline).
    public func exit(of child: String) async throws -> AppChildChunk.Exit? {
        let request = AppChildRequests.Read(child: child, after: .max, waitMS: 0, untilExit: true)
        return try await connection.request(request, timeout: nil).exit
    }

    /// Stops `child` with `mode`; a child that already ended is not an error.
    public func stop(_ child: String, mode: AppChildStopMode = .terminate) async throws {
        _ = try await connection.request(AppChildRequests.Stop(child: child, mode: mode.rawValue))
    }

    /// The daemon's children (this app's, from before a crash too), oldest first, or only `owner`'s.
    public func list(owner: String? = nil) async throws -> [AppChildRow] {
        try await connection.request(AppChildRequests.List(owner: owner)).children
    }

    /// Ends every child at once (a clean quit); returns how many were running.
    @discardableResult
    public func releaseAll() async throws -> Int {
        try await connection.request(AppChildRequests.ReleaseAll()).stopped
    }

    static func milliseconds(_ duration: Duration) -> UInt64 {
        let (seconds, attoseconds) = duration.components
        let (scaled, overflow) = seconds.multipliedReportingOverflow(by: 1000)
        return overflow ? .max : UInt64(clamping: scaled &+ attoseconds / 1_000_000_000_000_000)
    }
}

/// One child for ``AppChildClient/start(_:)``.
public struct AppChildSpec: Sendable, Equatable {
    /// A label for diagnostics (`app-child-list`).
    public var name: String
    /// The app's tag for the child (its pane, its service), so a relaunched app finds its own.
    public var owner: String
    public var executable: URL
    public var arguments: [String]
    /// The child's whole environment: none of the daemon's is inherited.
    public var environment: [String: String]
    public var workingDirectory: URL?
    /// stdin is a pipe the daemon keeps open (the child sees its end when it is stopped or the daemon
    /// ends) instead of /dev/null.
    public var lifeline: Bool
    /// The daemon writes a fresh 64-hex-character secret as the lifeline's first line.
    public var secret: Bool
    /// The start answers once an output line contains this.
    public var ready: String?
    public var readyTimeout: Duration?

    public init(name: String, owner: String, executable: URL, arguments: [String] = [], environment: [String: String] = [:],
                workingDirectory: URL? = nil, lifeline: Bool = false, secret: Bool = false, ready: String? = nil,
                readyTimeout: Duration? = nil) {
        self.name = name
        self.owner = owner
        self.executable = executable
        self.arguments = arguments
        self.environment = environment
        self.workingDirectory = workingDirectory
        self.lifeline = lifeline
        self.secret = secret
        self.ready = ready
        self.readyTimeout = readyTimeout
    }
}

/// How ``AppChildClient/stop(_:mode:)`` ends a child (always its whole process group).
public enum AppChildStopMode: String, Sendable {
    /// SIGINT as Ctrl-C does, then SIGTERM and SIGKILL 2 s apart.
    case interrupt
    /// SIGHUP as a closed terminal does.
    case hangup
    /// The lifeline closes and SIGTERM goes, then SIGKILL after 5 s.
    case terminate
}
