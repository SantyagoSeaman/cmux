@testable import CmuxNextApp
import CmuxNextBrowser
@testable import CmuxNextDaemon
import CmuxNextInput
import CmuxNextLaunch
import CmuxNextWakeups
import CmuxNextWakeupsTestSupport
import Foundation
import Testing

/// p08-startup-090: with a ``RestoredPageLoader`` (feature `restoredPageLoader`) every Chromium page the launch
/// restored waits as its deferred page (no notice) for its turn, on the one deferral path: the ones that show a
/// few at a time, the next at a first content frame; a hidden one never, not even at startup complete, until it
/// shows; a click on a waiting page starts it. The crash-recovery deferral is unchanged.
@MainActor @Suite(.serialized, .timeLimit(.minutes(1))) struct RestoredPageLoaderTests {
    @Test func aShownRestoredPageWaitsForItsTurnThenStartsAsRestoreVisible() async throws {
        let f = LoaderPageFixture(cores: 2)
        let tab = try f.restore(5_001)[0]
        f.show(tab)
        let entry = try #require(f.cache.browser(for: tab))
        #expect(entry.tab is DeferredBrowserTab, "a shown restored page started before its turn")
        #expect(entry.chrome.noticeText == nil, "a launch deferral shows the crash notice")
        #expect(f.made.origins.isEmpty)

        f.turn()
        await f.ready(tab)
        #expect(f.made.origins == [.restoreVisible])
    }

    @Test func atMostTheCapStartsAndTheNextFollowsTheFirstContentFrame() async throws {
        let f = LoaderPageFixture(cores: 2)  // one at a time
        let tabs = try f.restore(5_010, 5_011, 5_012)
        for tab in tabs {
            f.show(tab)
            #expect(f.cache.browser(for: tab)?.tab is DeferredBrowserTab)
        }
        f.turn()
        await f.ready(tabs[0])
        f.turn()
        #expect(f.made.pages.count == 1, "a second page started while the first loaded")
        #expect(f.cache.restoredPages?.loadingCount == 1)

        f.made.pages[0].simulateFirstContentFrame()
        f.turn()
        await f.ready(tabs[1])
        #expect(f.made.pages.count == 2)
        #expect(f.made.origins == [.restoreVisible, .restoreVisible])
    }

    @Test func aHiddenRestoredPageDoesNotStartAtStartupCompleteOnlyOnceItShows() async throws {
        let f = LoaderPageFixture(cores: 8)
        let tab = try f.restore(5_020)[0]
        #expect(f.cache.browser(for: tab)?.tab is DeferredBrowserTab)
        f.launch.complete()
        f.launch.fire()
        f.turn()
        #expect(f.made.origins.isEmpty, "a hidden restored page loaded before it showed")

        f.show(tab)
        f.turn()
        await f.ready(tab)
        #expect(f.made.origins == [.user], "after completion no origin waits")
    }

    @Test func aClickOnAWaitingPageStartsItPastAFullCap() async throws {
        let f = LoaderPageFixture(cores: 2)
        let restored = try f.restore(5_030, 5_031)
        let first = restored[0], clicked = restored[1]
        f.show(first)
        f.show(clicked)
        _ = f.cache.browser(for: first)
        let waiting = try #require(f.cache.browser(for: clicked)?.tab as? DeferredBrowserTab)
        f.turn()
        await f.ready(first)
        #expect(f.made.pages.count == 1)

        waiting.setFocused(true)  // the person clicked its pane (FocusEffectApplier)
        #expect(f.made.pages.count == 1, "the click started the page inside the focus change's turn")
        f.turn()
        await f.ready(clicked)
        #expect(f.made.pages.count == 2)
    }

    @Test func theCrashRecoveryDeferralIsUnchanged() async throws {
        let f = LoaderPageFixture(cores: 8)
        f.cache.defersRestoredPages = true
        let tab = try f.restore(5_040)[0]
        f.show(tab)
        let entry = try #require(f.cache.browser(for: tab))
        #expect(entry.tab is DeferredBrowserTab)
        #expect(entry.chrome.noticeText != nil, "a crash deferral lost its notice")
        f.turn()
        #expect(f.made.origins.isEmpty, "the loader started a page that waits for the person")
        #expect(f.cache.restoredPages?.isWaiting(tab.id) == false)

        entry.tab.reload()  // the person's reload, as before
        await f.ready(tab)
        #expect(f.made.pages.count == 1)
        #expect(f.cache.restoredPages?.loadingCount == 0, "a crash deferral's start took a loader slot")
    }
}

@MainActor
private final class LoaderPageFixture {
    let launch = StartupTestScheduler()
    let daemon = DaemonService()
    let cache: TabContentCache
    let made = MadePages()
    private let pane = VisibilityNode(root: "pane", own: .visible)
    /// The selected tabs' nodes, held as `PaneController.selectedTabNode` holds them (the cache holds them weakly).
    private var shown: [String: VisibilityNode] = [:]
    private var posted: [@MainActor () -> Void] = []
    private var waiting: [String: CheckedContinuation<Void, Never>] = [:]
    private var arrived: Set<String> = []

    init(cores: Int) {
        let engine = CEFEngine(source: CEFRuntimeSource(layout: nil), stores: .inMemory(), signInCallbacks: CEFSignInCallbacks(),
                               input: InputEventTap(), launch: launch.startup)
        cache = TabContentCache(daemon: daemon, chromium: ChromiumEngineSlot(engine), fileInput: NoFileInput(), mainWork: linklessMainWork())
        cache.browserTabs.cefUnavailable = { nil }
        cache.chromiumNow.runs = { false }
        cache.makeCEFTab = { [made] configuration in
            let page = MockBrowserEngine(kind: .cef, completesNavigationsImmediately: false).makeMockTab(configuration)
            made.origins.append(configuration.origin)
            made.pages.append(page)
            return page
        }
        cache.onBrowserReady = { [weak self] key in self?.arrive(key) }
        cache.restoredPages = RestoredPageLoader(cores: cores, pressure: nil, focus: { _ in .unfocused },
                                                 post: { [weak self] work in self?.posted.append(work) },
                                                 makeTimer: { IdleTimer() })
    }

    /// Chromium tabs in the daemon's launch snapshot, one per surface: restored tabs.
    func restore(_ surfaces: UInt64...) throws -> [TabModel] {
        let tabs = surfaces.map { surface in
            TabSnapshot(surface: SurfaceID(rawValue: surface), kind: .browser, url: "https://example.com/",
                        browserRenderer: "frontend", browserEngine: "cef")
        }
        let screen = ScreenSnapshot(id: 1, layout: .leaf(1), panes: [PaneSnapshot(id: 1, tabs: tabs)])
        daemon.store.applyProvisional(snapshot: DaemonTree(workspaces: [WorkspaceSnapshot(id: 1, key: nil, name: "w", screens: [screen])]))
        return try surfaces.map { surface in
            let restored = try #require(daemon.store.tab(surface: SurfaceID(rawValue: surface)))
            #expect(daemon.store.restoredTabIDs.contains(restored.id))
            return restored
        }
    }

    /// Selects `tab` in a visible pane (`PaneController.selectTabNode`).
    func show(_ tab: TabModel) {
        let node = cache.node(for: tab.id)
        try? node.attach(to: pane)
        node.set(.visible)
        shown[tab.id] = node
    }

    /// Runs the loader's posted turns.
    func turn() {
        while !posted.isEmpty {
            let work = posted
            posted = []
            for item in work { item() }
        }
    }

    /// Returns once `tab`'s Chromium page was installed (`onBrowserReady`).
    func ready(_ tab: TabModel) async {
        if arrived.remove(tab.id) != nil { return }
        await withCheckedContinuation { waiting[tab.id] = $0 }
    }

    /// A re-show with the page made (not the deferred page's start, which re-shows before its page exists).
    private func arrive(_ key: String) {
        guard let page = cache.existingBrowser(key)?.tab, !(page is DeferredBrowserTab) else { return }
        if let waiter = waiting.removeValue(forKey: key) { waiter.resume() } else { arrived.insert(key) }
    }
}

@MainActor
private final class MadePages {
    var origins: [BrowserTabOrigin] = []
    var pages: [MockBrowserTab] = []
}

/// A loader timeout that never fires by itself (the tests end each load by its first content frame).
@MainActor
private final class IdleTimer: RestoredPageTimer {
    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void) {}
    func cancel() {}
}
