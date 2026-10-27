import CmuxNextCompat

/// Where a service built before the local daemon connects finds the daemon's app children
/// (`appChildrenInDaemon`, p01-process-239): the composition root sets the resolver once, and each use
/// asks it for the current connection's ``AppChildServing``. No resolver (the feature off): nil.
public final class AppChildSlot: Sendable {
    public typealias Resolver = @MainActor @Sendable () -> (any AppChildServing)?

    // Set once at launch and read per use; the lock is held only to copy the closure.
    private let resolver = Mutex<Resolver?>(nil)

    public init() {}

    /// Sets how the children are found (the composition root, when the feature is on).
    public func resolve(with resolver: @escaping Resolver) {
        self.resolver.withLock { $0 = resolver }
    }

    /// The local daemon's app children now, or nil, for main-actor code.
    @MainActor public var children: (any AppChildServing)? {
        resolver.withLock { $0 }?()
    }

    /// The local daemon's app children now, or nil.
    public func current() async -> (any AppChildServing)? {
        guard let resolve = resolver.withLock({ $0 }) else { return nil }
        return await resolve()
    }
}
