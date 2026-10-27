import CmuxNextCompat

/// Where a service built before the local daemon connects finds the daemon's app children
/// (`appChildrenInDaemon`, p01-process-239): the composition root sets the resolver once, and each use
/// asks it for the current connection's ``AppChildServing``. No resolver (the feature off): nil.
public final class AppChildSlot: Sendable {
    public typealias Resolver = @MainActor @Sendable () -> (any AppChildServing)?
    /// Returns once the daemon's first connection exists or its startup gave up (the daemon service's own
    /// startup deadline bounds it); at once when the app is connected, or reconnecting after its first.
    public typealias FirstConnection = @MainActor @Sendable () async -> Void

    private struct Resolution {
        let resolve: Resolver
        let firstConnection: FirstConnection
    }

    // Set once at launch and read per use; the lock is held only to copy the closures.
    private let resolution = Mutex<Resolution?>(nil)

    public init() {}

    /// Sets how the children are found (the composition root, when the feature is on). `firstConnection` lets a
    /// use that comes before the daemon's handshake (a VS Code tab restored at launch) wait for it, so it is not
    /// taken for the feature off.
    public func resolve(with resolver: @escaping Resolver, firstConnection: @escaping FirstConnection = {}) {
        resolution.withLock { $0 = Resolution(resolve: resolver, firstConnection: firstConnection) }
    }

    /// The local daemon's app children now, or nil, for main-actor code.
    @MainActor public var children: (any AppChildServing)? {
        resolution.withLock { $0 }?.resolve()
    }

    /// The local daemon's app children, after its first connection when the app is still making it; nil with the
    /// feature off, or when the daemon could not be reached.
    public func current() async -> (any AppChildServing)? {
        guard let resolution = resolution.withLock({ $0 }) else { return nil }
        await resolution.firstConnection()
        return await resolution.resolve()
    }

    /// A clean quit: ends every child this app has in the daemon at once (bounded,
    /// ``AppChildClient/releaseWait``), so none waits out the daemon's grace. It never waits for a first
    /// connection: an app that never connected started no child. Nothing with the feature off or an older
    /// daemon; a failure leaves the grace to end them.
    public func releaseAllForQuit() async {
        guard let resolve = resolution.withLock({ $0 })?.resolve, let children = await resolve(),
              await children.isServed else { return }
        _ = try? await children.releaseAll()
    }
}
