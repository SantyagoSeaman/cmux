public import CmuxNextCompat
public import CmuxNextWakeups

/// Loads the browser pages a launch restored a few at a time, as Chrome's `TabLoader` does
/// (chrome/browser/sessions/tab_loader.cc), on the App's one deferral path (`TabContentCache+Deferred`):
/// a restored page waits as its deferred page until the loader gives it its turn (p08-startup.md section 3D,
/// task 090, feature `restoredPageLoader`).
///
/// - Order: the focused page of the key window first, then the focused pages of the other windows, then
///   the other pages that show, each tier in the order the pages asked (Chrome's `SessionRestorePolicy`
///   scores the active tab of each window first). Focus is read at each turn, so a focus that moves while
///   pages wait moves the order.
/// - Cap: ``simultaneousLoads(cores:)`` pages load at once, Chrome's `CalculateSimultaneousTabLoads`
///   (chrome/browser/sessions/session_restore_policy.cc: one load per 2 cores, at least 1, at most 4).
/// - Memory pressure: while the pressure vote is above normal, one page loads at a time. Chrome stops
///   loading under pressure (`TabLoader::OnMemoryPressure`); cmux goes on one at a time, so the pages that
///   show still load.
/// - Next: a loading page frees its slot at its first content frame, or ``loadTimeout`` after its start,
///   whichever comes first (`TabLoader`'s force-load timer), or when it closes.
/// - Hidden: a page that does not show never loads until it shows; it keeps its place meanwhile.
/// - Click: a page the person focuses (``promote(_:)``) starts at its next turn even when every slot is
///   taken, as Chrome loads an activated tab at once, outside its `TabLoader`.
///
/// Every start goes through one posted turn (``schedulePump()``): the pages one layout pass asks for are
/// ordered together, and no start runs inside the caller's turn (a focus change, a first paint).
///
/// ```swift
/// let loader = RestoredPageLoader(cores: ProcessInfo.processInfo.activeProcessorCount, pressure: memoryCoordinator,
///                                 focus: { focus(of: $0) }, post: { AfterFrameCommit().run($0) },
///                                 makeTimer: { MainDeadline(owner: "RestoredPageLoader.next", workClass: .visible, scheduler: deadlines) })
/// loader.enqueue(key, node: tabNode) { deferredPage.reload() }  // the start runs at the page's turn
/// loader.didStart(key)                                       // any start of the page (its turn, a reload, a command)
/// loader.track(key) { page.whenFirstContentFrame($0) }       // the live page: its first content frees the slot
/// ```
@MainActor
public final class RestoredPageLoader {
    /// Where a waiting page has the focus, in load order.
    public enum Focus: Int, Sendable {
        /// The focused content of the key window.
        case keyWindow
        /// The focused content of another window.
        case window
        /// No window focuses it.
        case unfocused
    }

    /// How long a loading page holds its slot when it shows no content (`TabLoader`'s force-load timer).
    public nonisolated static let loadTimeout: Duration = .seconds(3)

    /// The pages that load at once on a Mac with `cores` cores and no memory pressure: one per 2 cores, at
    /// least 1, at most 4 (`CalculateSimultaneousTabLoads`, chrome/browser/sessions/session_restore_policy.cc).
    public nonisolated static func simultaneousLoads(cores: Int) -> Int { min(4, max(1, cores / 2)) }

    /// The pages that load at once with no memory pressure (``simultaneousLoads(cores:)``).
    public let cap: Int
    private let focus: @MainActor (String) -> Focus
    private let post: @MainActor (@escaping @MainActor () -> Void) -> Void
    private let makeTimer: @MainActor () -> any RestoredPageTimer
    private let pressure: (any MemoryPressureVoteSource)?
    private var pressureWatch: MainActorHandlerRegistration<MemoryPressureLevel>?
    private var waiting: [String: Waiting] = [:]
    private var loading: [String: Load] = [:]
    private var nextOrder: UInt64 = 0
    private var nextLoad: UInt64 = 0
    private var pumpPosted = false

    private struct Waiting {
        let order: UInt64
        weak var node: VisibilityNode?
        let start: @MainActor () -> Void
        /// Asks for a turn each time the page shows; released with the entry.
        let watch: VisibilitySubscription?
        var promoted = false
    }

    private struct Load {
        let id: UInt64
        let timer: any RestoredPageTimer
    }

    /// - Parameters:
    ///   - cores: This Mac's active cores (``simultaneousLoads(cores:)``).
    ///   - pressure: The memory pressure vote; above normal, one page loads at a time. Nil: never under pressure.
    ///   - focus: Where tab `key` has the focus now; read at each turn.
    ///   - post: Runs the loader's turn later on the main thread (the App: after the current frame committed).
    ///   - makeTimer: One loading page's timeout (the App: a ``MainDeadline``).
    public init(cores: Int, pressure: (any MemoryPressureVoteSource)?, focus: @escaping @MainActor (String) -> Focus,
                post: @escaping @MainActor (@escaping @MainActor () -> Void) -> Void,
                makeTimer: @escaping @MainActor () -> any RestoredPageTimer) {
        cap = Self.simultaneousLoads(cores: cores)
        (self.focus, self.post, self.makeTimer, self.pressure) = (focus, post, makeTimer, pressure)
        // Pressure that ends frees the slots it held back.
        pressureWatch = pressure?.observePressureVote { [weak self] _ in self?.schedulePump() }
    }

    /// The pages that may load at once now: ``cap``, or 1 under memory pressure.
    public var limit: Int { (pressure?.pressureVote ?? .normal) == .normal ? cap : 1 }

    /// The pages loading now (started, no first content yet, inside their timeout).
    public var loadingCount: Int { loading.count }

    /// Whether tab `key`'s page waits for its turn.
    public func isWaiting(_ key: String) -> Bool { waiting[key] != nil }

    /// Tab `key`'s restored page waits for its turn; `start` starts it then. It asks for a turn now when
    /// `node` shows, else each time `node` shows, never while it does not show.
    public func enqueue(_ key: String, node: VisibilityNode, start: @escaping @MainActor () -> Void) {
        guard waiting[key] == nil, loading[key] == nil else { return }
        let watch = node.subscribe(owner: "RestoredPageLoader") { [weak self] visibility in
            if visibility == .visible { self?.schedulePump() }
        }
        waiting[key] = Waiting(order: nextOrder, node: node, start: start, watch: watch)
        nextOrder &+= 1
        if node.isVisible { schedulePump() }
    }

    /// The person focused tab `key`'s waiting page: it starts at the next turn, even when every slot is taken.
    public func promote(_ key: String) {
        guard waiting[key] != nil else { return }
        waiting[key]?.promoted = true
        schedulePump()
    }

    /// Tab `key`'s page started, by its turn or by anything else (the person's reload, a command): it leaves
    /// the queue and holds a slot until its first content frame or its timeout. A tab the loader does not
    /// know (a crash deferral) is left alone.
    public func didStart(_ key: String) {
        guard waiting.removeValue(forKey: key) != nil else { return }
        begin(key)
    }

    /// Tab `key`'s live page: `firstContent` runs its argument once, at the page's first content frame or when
    /// the page closes first; either frees the slot. Nothing for a page that does not hold a slot.
    public func track(_ key: String, firstContent: @MainActor (@escaping @MainActor (Bool) -> Void) -> Void) {
        guard let id = loading[key]?.id else { return }
        firstContent { [weak self] _ in self?.finish(key, id) }
    }

    /// Tab `key` closed: it leaves the queue, and its slot goes to the next page.
    public func forget(_ key: String) {
        waiting[key] = nil
        guard let load = loading.removeValue(forKey: key) else { return }
        load.timer.cancel()
        schedulePump()
    }

    // MARK: Turns

    private func schedulePump() {
        guard !pumpPosted else { return }
        pumpPosted = true
        post { [weak self] in self?.pump() }
    }

    /// Starts the waiting pages whose turn it is: the promoted ones, then, while a slot is free, the best
    /// placed page that shows.
    private func pump() {
        pumpPosted = false
        while let key = nextToStart(), let entry = waiting.removeValue(forKey: key) {
            begin(key)
            entry.start()
        }
    }

    private func nextToStart() -> String? {
        let hasSlot = loading.count < limit
        var best: (key: String, rank: (Int, Int, UInt64))?
        for (key, entry) in waiting where entry.promoted || (hasSlot && entry.node?.isVisible == true) {
            let rank = (entry.promoted ? 0 : 1, focus(key).rawValue, entry.order)
            if let current = best, current.rank <= rank { continue }
            best = (key, rank)
        }
        return best?.key
    }

    private func begin(_ key: String) {
        guard loading[key] == nil else { return }
        nextLoad &+= 1
        let id = nextLoad, timer = makeTimer()
        loading[key] = Load(id: id, timer: timer)
        timer.schedule(after: Self.loadTimeout) { [weak self] in self?.finish(key, id) }
    }

    private func finish(_ key: String, _ id: UInt64) {
        guard loading[key]?.id == id, let load = loading.removeValue(forKey: key) else { return }
        load.timer.cancel()
        schedulePump()
    }
}

/// One loading page's timeout in ``RestoredPageLoader``: a ``MainDeadline`` in the App, a hand-fired timer in tests.
@MainActor
public protocol RestoredPageTimer: AnyObject {
    /// Runs `action` once, no sooner than `delay` from now, replacing a pending one.
    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void)
    /// Drops the pending action.
    func cancel()
}

extension MainDeadline: RestoredPageTimer {}
