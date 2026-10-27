/// The daemon's app children as a seam (``AppChildClient`` is the real one; tests pass a fake).
public protocol AppChildServing: Sendable {
    /// Whether the daemon serves app children (an older daemon does not).
    var isServed: Bool { get async }
    func start(_ spec: AppChildSpec) async throws -> AppChildStarted
    func read(_ child: String, after: UInt64, wait: Duration) async throws -> AppChildChunk
    func exit(of child: String) async throws -> AppChildChunk.Exit?
    func stop(_ child: String, mode: AppChildStopMode) async throws
    func list(owner: String?) async throws -> [AppChildRow]
}

extension AppChildClient: AppChildServing {}
