import CmuxNextActions
import CmuxNextControl
import CmuxNextDaemon
import CmuxNextDesign
import CmuxNextSettings
import CmuxNextWakeups
import Foundation
#if DEBUG || CMUX_BENCH_HOOKS
import CmuxNextOracle
#endif

/// The headless process's one composition root (plans/cmux-next/chrome-parity/p15-oracle.md section H, task
/// 160): the action registry, the settings controller, one daemon store mirroring a cmux-tui owner, and the control
/// socket over them, with no window, view or terminal renderer. Chromium's headless shell composes the content layer
/// the same way, without the browser UI (headless/lib/browser/headless_browser_impl.cc, `HeadlessBrowserImpl`).
///
/// Every owner is a stored property built here, once per process (main.swift); nothing is a shared instance
/// (p15-oracle.md section 4, rule 4). It serves the router's built-in methods (`snapshot.get`, `action.run`,
/// `action.list`, `settings.*`), the `after: "sync"` barrier, and in DEV and bench builds the oracle's
/// `debug.oracle.*` methods over its own facet list (``makeOracleFacets()``).
///
/// ```swift
/// let services = try await HeadlessServices.start(options: options, identity: identity)
/// await services.stop()
/// ```
@MainActor
final class HeadlessServices {
    let mainWork: MainWorkScheduler
    let registry: ActionRegistry
    let settings: SettingsController
    /// The mirror of the daemon at ``HeadlessOptions/daemonSocket``.
    let store: DaemonStore
    let control: ControlService
    #if DEBUG || CMUX_BENCH_HOOKS
    /// The registry's perform-turn observer: `debug.oracle.recorder` and the `intents` facet read it.
    let oracleTurns: OracleActionTurns
    #endif
    private let daemonSocket: String?
    /// The connection the store mirrors, once the first connect succeeded.
    private var connection: DaemonConnection?
    /// The first connect and then the store's mirror; cancelled by ``stop()``.
    private var mirror: Task<Void, Never>?
    /// Publishes the control snapshot when what it shows changed.
    private var published: ServiceObservation<HeadlessSnapshot>?

    /// Builds every owner, loads the settings, registers the methods, publishes the snapshot, starts the daemon
    /// mirror and binds the control socket. No request is read before every method is registered
    /// (``ControlService/serve(adopting:)``).
    ///
    /// - Parameters:
    ///   - options: The launch arguments.
    ///   - identity: What `system.identify` reports.
    ///   - managedReader: The managed (MDM) preferences layer; tests pass an empty one.
    ///   - managedWatchFiles: The files whose changes mean a profile changed.
    /// - Throws: ``ControlSocketServer/StartError``: `disabled` when the resolved access mode is `off`, or why the
    ///   socket does not bind.
    static func start(options: HeadlessOptions, identity: ControlIdentity,
                      managedReader: any ManagedPreferenceReader = ManagedPreferenceLocation.defaultReader(),
                      managedWatchFiles: [URL] = ManagedPreferenceLocation.watchedFiles()) async throws -> HeadlessServices {
        let mainWork = MainWorkScheduler(budget: MainFrameBudget())
        #if DEBUG || CMUX_BENCH_HOOKS
        let turns = OracleActionTurns()
        let registry = ActionRegistry.standard(runTiming: NoActionRunTiming(), turnObserver: turns, mainWork: mainWork)
        #else
        let registry = ActionRegistry.standard(runTiming: NoActionRunTiming(), mainWork: mainWork)
        #endif
        let settings = SettingsController(registry: registry, design: DesignSettings(), fileURL: options.configFile,
                                          managedReader: managedReader, managedWatchFiles: managedWatchFiles)
        // The first load reads the file and the managed layer off the main thread (`reload`), then the watches start;
        // nothing is served before it, so the access mode and the first snapshot are the file's.
        await settings.reload()
        settings.start()
        let configured = settings.snapshot.root.value(at: ["automation", "socketControlMode"])?.stringValue
        let mode = ControlService.resolveAccessMode(explicit: options.accessMode, environment: options.environment, configured: configured)
        let control = try ControlService.prepare(registry: registry, settingsStore: settings.file, settingsWriter: settings,
                                                 configuration: ControlSocketServer.Configuration(path: options.controlSocket, accessMode: mode),
                                                 identity: identity, mainWork: mainWork)
        #if DEBUG || CMUX_BENCH_HOOKS
        let services = HeadlessServices(mainWork: mainWork, registry: registry, settings: settings, control: control,
                                        daemonSocket: options.daemonSocket, turns: turns)
        #else
        let services = HeadlessServices(mainWork: mainWork, registry: registry, settings: settings, control: control,
                                        daemonSocket: options.daemonSocket)
        #endif
        do {
            try services.serve()
        } catch {
            await services.stop()
            throw error
        }
        return services
    }

    #if DEBUG || CMUX_BENCH_HOOKS
    private init(mainWork: MainWorkScheduler, registry: ActionRegistry, settings: SettingsController, control: ControlService,
                 daemonSocket: String?, turns: OracleActionTurns) {
        (self.mainWork, self.registry, self.settings, self.control) = (mainWork, registry, settings, control)
        self.daemonSocket = daemonSocket
        oracleTurns = turns
        store = DaemonStore(waitDeadlines: mainWork.wakeUps, causes: mainWork.causes)
    }
    #else
    private init(mainWork: MainWorkScheduler, registry: ActionRegistry, settings: SettingsController, control: ControlService,
                 daemonSocket: String?) {
        (self.mainWork, self.registry, self.settings, self.control) = (mainWork, registry, settings, control)
        self.daemonSocket = daemonSocket
        store = DaemonStore(waitDeadlines: mainWork.wakeUps, causes: mainWork.causes)
    }
    #endif

    private func serve() throws {
        #if DEBUG || CMUX_BENCH_HOOKS
        registry.willRunObservers.append { [turns = oracleTurns] _, _ in turns.runStarted() }
        registry.refusalObserver = { [turns = oracleTurns] _, _ in turns.noteRefusal() }
        control.router.register(OracleControl.methods(host: self, snapshots: control.router.snapshots, turns: oracleTurns))
        #endif
        registerSyncBarrier()
        // Before the first publish: without a daemon socket the first snapshot already says `failed`.
        startMirror()
        publishSnapshots()
        try control.serve(adopting: nil)
    }

    /// Closes the socket, the daemon connection and the settings watches.
    func stop() async {
        control.stop()
        published?.cancel()
        mirror?.cancel()
        settings.stop()
        await connection?.close()
    }

    // MARK: - Snapshot

    /// What the control snapshot shows of this process: its daemon's topology and the effective settings.
    struct HeadlessSnapshot: Equatable {
        var topology: ControlTopology
        var settings: CmuxNextSettings.JSONValue
    }

    /// The topology of `store` with no window: each pane shows the daemon's default tab, the selection a window
    /// would start from (``PaneModel/defaultTabIndex``).
    static func topology(of store: DaemonStore) -> ControlTopology {
        var topology = ControlTopology(store: store, selectedTab: { pane in
            pane.tabs.indices.contains(pane.defaultTabIndex) ? pane.tabs[pane.defaultTabIndex].id : nil
        })
        topology.daemonSequence = store.appliedSequence
        if case .failed(let message) = store.connectionState { topology.daemonFailure = message }
        return topology
    }

    /// The app's publish order without its windows (CmuxNextApp ControlSnapshotPublisher): a change of what the
    /// snapshot shows publishes once in an automation task; a control work-queue frame (an `action.run`) marks the
    /// snapshot stale, so the next read builds it first and sees its own write.
    private func publishSnapshots() {
        let (store, settings, snapshots) = (store, settings, control.router.snapshots)
        let read: @MainActor () -> HeadlessSnapshot = { HeadlessSnapshot(topology: Self.topology(of: store), settings: settings.snapshot.root) }
        let publish: @MainActor @Sendable (HeadlessSnapshot) -> Void = { value in
            snapshots.publish(rebuilt: true) { snapshot in
                snapshot.topology = value.topology
                snapshot.settings = value.settings
            }
        }
        snapshots.setRefresher({ publish(HeadlessSnapshot(topology: Self.topology(of: store), settings: settings.snapshot.root)) })
        control.router.workQueue.setAfterFrame { snapshots.markStale() }
        published = ServiceObservation(.automation, owner: "headless.control.snapshot", on: mainWork, read: read, apply: publish)
    }

    // MARK: - Daemon

    /// `after: "sync"`: one round trip to the daemon, then the event sequence routed before its reply (the app's
    /// barrier, AppControl+Barrier.swift).
    private func registerSyncBarrier() {
        control.router.registerSyncBarrier { [weak self, work = mainWork] in
            guard let connection = await work.apply(.automation, owner: "headless.sync_barrier", { self?.connection }) else {
                throw ControlError(code: "unavailable", message: DaemonError.notConnected.description)
            }
            _ = try await connection.request(IdentifyRequest())
            guard let sequence = await connection.eventSequence() else {
                throw ControlError(code: "unavailable", message: DaemonError.notConnected.description)
            }
            return sequence
        }
    }

    private func startMirror() {
        guard let socket = daemonSocket else {
            store.markFailed(DaemonError.notConnected.description)
            return
        }
        mirror = HeadlessDaemonMirror(endpoint: DaemonEndpoint(socketPath: socket), store: store).start { [weak self] connection in
            await self?.adopt(connection)
        }
    }

    /// The connection the sync barrier and ``stop()`` use.
    private func adopt(_ connection: DaemonConnection) {
        self.connection = connection
    }
}

#if DEBUG || CMUX_BENCH_HOOKS
/// The headless facet list: what this process owns. The router adds `topology` itself (``OracleTopologyFacet``);
/// facets of windows, views, pages and terminals answer not-comparable here, so the headless-vs-app diff
/// (p15-oracle-161) classifies them as missing facets, never as passes.
extension HeadlessServices: OracleHost {
    func makeOracleFacets() -> OracleFacets {
        OracleFacets([
            OracleIntentsFacet(store: store, turns: oracleTurns),
            // The settings the controller applied and the motion arm as the gates compute it from them, as the app's.
            OracleSettingsFacet { [weak settings = self.settings] in
                guard let settings else { return nil }
                let applied = settings.applied, motion = Motion.basePolicy(of: settings.design)
                return OracleSettingsFacet.Reading(effectiveSha256: applied.effectiveSha256, nonDefaultKeys: applied.nonDefaultKeys(),
                                                   motionEnabled: motion.animationsEnabled, motionSpeed: motion.speed.timeScale,
                                                   reduceMotion: motion.reduceMotion)
            },
        ])
    }
}
#endif
