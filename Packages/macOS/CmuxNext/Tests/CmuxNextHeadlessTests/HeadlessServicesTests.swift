import CmuxNextControl
@testable import CmuxNextHeadless
import CmuxNextSettings
import Foundation
import Testing

/// The headless composition root (p15-oracle-160): with no window it binds the control socket and serves
/// `snapshot.get`, `action.run` (its window-free settings actions) and, in DEV and bench builds,
/// `debug.oracle.snapshot` over its own registry, settings and daemon store. Without a daemon socket the topology says `failed`, never an empty connected tree.
/// Run: `swift test --filter CmuxNextHeadlessTests` (on Linux: scripts/cmux-next/linux/swift-tier.sh).
@MainActor @Suite(.serialized) struct HeadlessServicesTests {
    private typealias Value = CmuxNextSettings.JSONValue

    /// Services bound on a socket under a fresh short directory (sun_path holds 104 bytes on macOS, 108 on Linux).
    private func started() async throws -> (HeadlessServices, URL) {
        let directory = URL(fileURLWithPath: "/tmp", isDirectory: true).appending(path: "hl-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let config = directory.appending(path: "cmux.json")
        try Data("{}".utf8).write(to: config)
        let options = try HeadlessOptions(arguments: ["--socket", directory.appending(path: "c.sock").path, "--config", config.path,
                                                      "--access-mode", "automation"], environment: [:])
        let identity = ControlIdentity(appName: "cmux-next-headless", version: "0", build: "0", bundleID: nil, tag: nil, processID: getpid())
        let services = try await HeadlessServices.start(options: options, identity: identity,
                                                        managedReader: FixedManagedPreferenceReader(ManagedPreferences()), managedWatchFiles: [])
        return (services, directory)
    }

    private func call(_ services: HeadlessServices, _ method: String, _ params: [String: Value] = [:]) async -> Result<Value, ControlError> {
        await services.control.router.handle(ControlRequest(id: "1", method: method, params: params))
    }

    @Test func snapshotGetAnswersWithTheDaemonFailedWhenNoDaemonIsGiven() async throws {
        let (services, directory) = try await started()
        defer { try? FileManager.default.removeItem(at: directory) }
        #expect(FileManager.default.fileExists(atPath: services.control.socketPath))

        let topology = try await call(services, "snapshot.get").get()["topology"]
        #expect(topology?["daemon_state"] == "failed")
        #expect(topology?["loaded"] == false)
        #expect(topology?["workspaces"] == .array([]))
        await services.stop()
        #expect(!FileManager.default.fileExists(atPath: services.control.socketPath))
    }

    @Test func actionRunGoesThroughTheRegistrysCatalog() async throws {
        let (services, directory) = try await started()
        defer { try? FileManager.default.removeItem(at: directory) }

        let listed = try await call(services, "action.list").get()
        // The registry's catalog, published when the bridge attached: the standard one, not an empty list.
        #expect((listed["actions"]?.arrayValue?.count ?? 0) > 0)
        guard case .failure(let error) = await call(services, "action.run", ["action": "no.such.action"]) else {
            Issue.record("action.run ran an action the catalog does not have")
            return
        }
        #expect(error.code == "not_found")
        await services.stop()
    }

    @Test func aSettingsActionsWriteIsWhatTheNextReadSees() async throws {
        let (services, directory) = try await started()
        defer { try? FileManager.default.removeItem(at: directory) }

        // `tabs.overflow` is agent-settable, so a socket run may write it.
        _ = try await call(services, "action.run", ["action": "tabs.overflow.scroll"]).get()
        // No wait between the two: the run answered after its write, and the read builds the snapshot first.
        let read = try await call(services, "settings.get", ["path": "tabs.overflow"]).get()
        #expect(read["value"] == "scroll")
        let file = try JSONSerialization.jsonObject(with: Data(contentsOf: directory.appending(path: "cmux.json"))) as? [String: Any]
        #expect((file?["tabs"] as? [String: Any])?["overflow"] as? String == "scroll")
        await services.stop()
    }

    @Test func optionsTakeTheSocketFromTheEnvironmentAndRefuseUnknownFlags() throws {
        let options = try HeadlessOptions(arguments: ["--daemon-socket", "/tmp/d.sock"], environment: [LaunchIdentity.socketOverrideKey: "/tmp/c.sock"])
        #expect(options.controlSocket == "/tmp/c.sock")
        #expect(options.daemonSocket == "/tmp/d.sock")
        #expect(options.accessMode == nil)
        #expect(throws: HeadlessOptionsError.noSocket) { try HeadlessOptions(arguments: [], environment: [:]) }
        #expect(throws: HeadlessOptionsError.unknownArgument("--window")) {
            try HeadlessOptions(arguments: ["--socket", "/tmp/c.sock", "--window"], environment: [:])
        }
        #expect(throws: HeadlessOptionsError.missingValue("--socket")) { try HeadlessOptions(arguments: ["--socket"], environment: [:]) }
        #expect(throws: HeadlessOptionsError.badAccessMode("sometimes")) {
            try HeadlessOptions(arguments: ["--socket", "/tmp/c.sock", "--access-mode", "sometimes"], environment: [:])
        }
    }

    #if DEBUG || CMUX_BENCH_HOOKS
    @Test func theOracleListsWhatTheHeadlessProcessOwns() async throws {
        let (services, directory) = try await started()
        defer { try? FileManager.default.removeItem(at: directory) }

        let catalog = try await call(services, "debug.oracle.snapshot", ["list": true]).get().arrayValue ?? []
        let provided = Set(catalog.filter { $0["provided"] == true }.compactMap { $0["facet"]?.stringValue })
        #expect(provided == ["topology", "intents", "settings"])

        let snapshot = try await call(services, "debug.oracle.snapshot", ["facets": ["settings", "topology", "windows"]]).get()
        #expect(snapshot["settings"]?["effectiveSha256"]?.stringValue?.hasPrefix("sha256:") == true)
        #expect(snapshot["settings"]?["nonDefaultKeys"] == .array([]))
        #expect(snapshot["topology"]?["topology"]?["daemon_state"] == "failed")
        #expect(snapshot["windows"]?["status"] == "not-comparable")
        await services.stop()
    }
    #endif
}
