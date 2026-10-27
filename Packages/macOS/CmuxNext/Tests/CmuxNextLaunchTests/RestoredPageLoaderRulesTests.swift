import CmuxNextCompat
import CmuxNextLaunch
import CmuxNextWakeups
import Testing

/// The restored-page loader's rules (p08-startup-090, Chrome's `TabLoader`), on hand-run turns, hand-fired timers
/// and a hand-set pressure vote: focus then visibility order, the cap from cores, one load under memory pressure,
/// the next page at a first content frame or the timeout, hidden pages never until they show, a click promotes.
@MainActor @Suite(.serialized, .timeLimit(.minutes(1))) struct RestoredPageLoaderRulesTests {
    @Test func theCapComesFromTheCoresOnePerTwoAtLeastOneAtMostFour() {
        #expect(RestoredPageLoader.simultaneousLoads(cores: 1) == 1)
        #expect(RestoredPageLoader.simultaneousLoads(cores: 2) == 1)
        #expect(RestoredPageLoader.simultaneousLoads(cores: 6) == 3)
        #expect(RestoredPageLoader.simultaneousLoads(cores: 8) == 4)
        #expect(RestoredPageLoader.simultaneousLoads(cores: 24) == 4)
    }

    @Test func nothingStartsInTheCallersTurn() {
        let f = LoaderFixture(cores: 8)
        f.add("a", shown: true)
        #expect(f.started.isEmpty, "a page started inside the turn that asked")
        f.turn()
        #expect(f.started == ["a"])
    }

    @Test func theFocusedPageOfTheKeyWindowGoesFirstThenOtherWindowsFocusThenTheShownOnes() {
        let f = LoaderFixture(cores: 2)  // one at a time: the order shows
        for key in ["shown1", "otherWindow", "shown2", "keyWindow"] { f.add(key, shown: true) }
        f.focus = ["keyWindow": .keyWindow, "otherWindow": .window]
        f.turn()
        #expect(f.started == ["keyWindow"])
        f.paint("keyWindow")
        f.turn()
        #expect(f.started == ["keyWindow", "otherWindow"])
        f.paint("otherWindow")
        f.turn()
        f.paint("shown1")
        f.turn()
        #expect(f.started == ["keyWindow", "otherWindow", "shown1", "shown2"], "the shown pages load in the order they asked")
    }

    @Test func atMostTheCapLoadsAtOnceTwelveShownPagesOverThreeWindows() {
        let f = LoaderFixture(cores: 8)
        let windows = (0..<3).map { VisibilityNode(root: "window\($0)", own: .visible) }
        for page in 0..<12 { f.add("page\(page)", parent: windows[page % 3]) }
        var most = 0
        while f.started.count < 12 {
            f.turn()
            most = max(most, f.loader.loadingCount)
            #expect(f.loader.loadingCount <= 4)
            guard let next = f.started.first(where: { !f.painted.contains($0) }) else { break }
            f.paint(next)
        }
        #expect(f.started.count == 12)
        #expect(most == 4)
    }

    @Test func underMemoryPressureOnePageLoadsAtATimeAndTheRestFollowWhenItEnds() {
        let f = LoaderFixture(cores: 8)
        f.pressure.set(.warning)
        for key in ["a", "b", "c"] { f.add(key, shown: true) }
        f.turn()
        #expect(f.started == ["a"])
        #expect(f.loader.limit == 1)
        f.pressure.set(.normal)
        f.turn()
        #expect(f.started == ["a", "b", "c"], "the end of pressure gave back the slots")
    }

    @Test func theNextPageStartsAtTheFirstContentFrame() {
        let f = LoaderFixture(cores: 2)
        f.add("a", shown: true)
        f.add("b", shown: true)
        f.turn()
        #expect(f.started == ["a"])
        f.turn()
        #expect(f.started == ["a"], "b started while a still loaded")
        f.paint("a")
        #expect(f.loader.loadingCount == 0)
        f.turn()
        #expect(f.started == ["a", "b"])
    }

    @Test func theNextPageStartsAfterTheTimeoutWhenThePageShowsNothing() {
        let f = LoaderFixture(cores: 2)
        f.add("a", shown: true)
        f.add("b", shown: true)
        f.turn()
        #expect(f.timers.last?.delay == RestoredPageLoader.loadTimeout)
        f.timers.last?.fire()
        f.turn()
        #expect(f.started == ["a", "b"])
        f.paint("a")  // its late first frame frees nothing more
        #expect(f.loader.loadingCount == 1)
    }

    @Test func aHiddenPageNeverLoadsUntilItShows() {
        let f = LoaderFixture(cores: 8)
        let node = f.add("hidden", shown: false)
        f.turn()
        #expect(f.started.isEmpty)
        #expect(f.loader.isWaiting("hidden"))
        node.set(.visible)
        f.turn()
        #expect(f.started == ["hidden"])
    }

    @Test func aPageThatHidesBeforeItsTurnWaitsUntilItShowsAgain() {
        let f = LoaderFixture(cores: 2)
        f.add("a", shown: true)
        let b = f.add("b", shown: true)
        f.turn()
        b.set(.hidden)
        f.paint("a")
        f.turn()
        #expect(f.started == ["a"], "a hidden page took the free slot")
        b.set(.visible)
        f.turn()
        #expect(f.started == ["a", "b"])
    }

    @Test func aClickPromotesAWaitingPagePastAFullCap() {
        let f = LoaderFixture(cores: 2)
        f.add("a", shown: true)
        f.add("b", shown: true)
        f.add("clicked", shown: true)
        f.turn()
        #expect(f.started == ["a"])
        f.loader.promote("clicked")
        #expect(f.started == ["a"], "a click started the page inside the focus change's turn")
        f.turn()
        #expect(f.started == ["a", "clicked"])
        #expect(f.loader.loadingCount == 2)
    }

    @Test func aStartFromElsewhereTakesASlotAndUnknownTabsAreLeftAlone() {
        let f = LoaderFixture(cores: 2)
        f.add("a", shown: true)
        f.add("b", shown: true)
        f.loader.didStart("b")  // a command or the person's reload
        f.loader.didStart("crashDeferred")
        #expect(f.loader.loadingCount == 1)
        f.turn()
        #expect(f.started.isEmpty, "the loader started a page while a command's page held the only slot")
        f.loader.forget("b")
        f.turn()
        #expect(f.started == ["a"])
    }

    @Test func aClosedPageLeavesTheQueue() {
        let f = LoaderFixture(cores: 8)
        f.add("a", shown: true)
        f.loader.forget("a")
        f.turn()
        #expect(f.started.isEmpty)
        #expect(!f.loader.isWaiting("a"))
    }
}

@MainActor
private final class LoaderFixture {
    let pressure = PressureVote()
    var focus: [String: RestoredPageLoader.Focus] = [:]
    private(set) var started: [String] = []
    private(set) var painted: Set<String> = []
    private(set) var timers: [HandTimer] = []
    private var posted: [@MainActor () -> Void] = []
    private var firstContent: [String: @MainActor (Bool) -> Void] = [:]
    private var nodes: [VisibilityNode] = []
    private let cores: Int
    private(set) lazy var loader: RestoredPageLoader = RestoredPageLoader(
        cores: cores, pressure: pressure, focus: { [unowned self] key in self.focus[key] ?? .unfocused },
        post: { [unowned self] work in self.posted.append(work) },
        makeTimer: { [unowned self] in
            let timer = HandTimer()
            self.timers.append(timer)
            return timer
        })

    init(cores: Int) {
        self.cores = cores
    }

    /// Tab `key` waits with its own node, shown or hidden.
    @discardableResult
    func add(_ key: String, shown: Bool) -> VisibilityNode {
        let node = VisibilityNode(root: "tab:\(key)", own: shown ? .visible : .hidden)
        addToLoader(key, node: node)
        return node
    }

    /// Tab `key` waits, shown in `parent` (a window).
    func add(_ key: String, parent: VisibilityNode) {
        addToLoader(key, node: VisibilityNode("tab:\(key)", own: .visible, parent: parent))
    }

    /// Runs the posted turns.
    func turn() {
        while !posted.isEmpty {
            let work = posted
            posted = []
            for item in work { item() }
        }
    }

    /// Tab `key`'s page shows its first content.
    func paint(_ key: String) {
        painted.insert(key)
        firstContent.removeValue(forKey: key)?(true)
    }

    private func addToLoader(_ key: String, node: VisibilityNode) {
        nodes.append(node)
        loader.enqueue(key, node: node) { [unowned self] in
            self.started.append(key)
            self.loader.track(key) { self.firstContent[key] = $0 }  // the live page, as the cache's install tracks it
        }
    }
}

@MainActor
private final class HandTimer: RestoredPageTimer {
    private(set) var delay: Duration?
    private var action: (@MainActor () -> Void)?

    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void) {
        self.delay = delay
        self.action = action
    }

    func cancel() { action = nil }

    func fire() {
        let pending = action
        action = nil
        pending?()
    }
}

@MainActor
private final class PressureVote: MemoryPressureVoteSource {
    private let handlers = MainActorHandlers<MemoryPressureLevel>()
    private(set) var pressureVote: MemoryPressureLevel = .normal

    func set(_ level: MemoryPressureLevel) {
        guard level != pressureVote else { return }
        pressureVote = level
        handlers.call(level)
    }

    func observePressureVote(_ handler: @escaping @MainActor @Sendable (MemoryPressureLevel) -> Void)
        -> MainActorHandlerRegistration<MemoryPressureLevel> {
        handlers.add(handler)
    }
}
