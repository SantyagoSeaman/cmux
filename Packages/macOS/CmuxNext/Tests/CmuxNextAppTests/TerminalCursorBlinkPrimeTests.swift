import CmuxNextActions
@testable import CmuxNextApp
import CmuxNextDesign
import CmuxNextFeatures
@testable import CmuxNextSettings
import CmuxNextTerminal
import Foundation
import Testing

/// The launch primes `terminal.cursorBlinkTimeout` from the feature list the services read
/// (`AppDelegate.applicationDidFinishLaunching`, p14-energy-069): with the `terminalCursorBlinkTimeout` feature on,
/// the first Ghostty config already has the timeout and the services' first follow reloads nothing; off, the
/// runtime keeps Ghostty's blink.
@MainActor @Suite(.serialized) struct TerminalCursorBlinkPrimeTests {
    /// Where the terminal setting hears app notices: held for the whole test (its AppEvents hold it weakly).
    private let center = NotificationCenter()

    private func features(enabled: Bool) -> FeatureList {
        FeatureList(registry: FeatureRegistry(), channel: .dev,
                    overrides: FeatureOverrides(environmentEnabled: enabled ? [Feature.terminalCursorBlinkTimeout.name] : []))
    }

    private func settings(_ json: String) async throws -> (SettingsController, URL) {
        let directory = FileManager.default.temporaryDirectory.appending(path: "cnc-cursor-blink-prime-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let url = directory.appending(path: "cmux.json")
        try Data(json.utf8).write(to: url)
        let settings = SettingsController(registry: ActionRegistry(catalog: []), design: DesignSettings(), fileURL: url,
                                          managedReader: FixedManagedPreferenceReader(ManagedPreferences()), managedWatchFiles: [])
        await settings.reload()
        return (settings, url)
    }

    /// Primes from `features` as the launch does, then follows with the services' setting built from the same
    /// list; returns the setting and its reload count.
    private func primeThenFollow(_ settings: SettingsController, features: FeatureList) -> (TerminalThemeSetting, Reloads) {
        TerminalThemeSetting.prime(settings.snapshot, features: features)
        let terminal = TerminalThemeSetting.testing(backdropScope: ThemeScope(level: .room), center: center,
                                                    cursorBlinkTimeout: features.isEnabled(.terminalCursorBlinkTimeout))
        let reloads = Reloads()
        terminal.reload = { reloads.count += 1 }
        terminal.follow(settings)
        return (terminal, reloads)
    }

    @MainActor private final class Reloads {
        var count = 0
    }

    @Test func theFeatureOnPrimesTheTimeoutAndTheFirstFollowDoesNotReload() async throws {
        let (settings, url) = try await settings(#"{"terminal": {"cursorBlinkTimeout": 30}}"#)
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        defer { GhosttyRuntime.shared.cursorBlinkTimeout = nil }
        let (terminal, reloads) = primeThenFollow(settings, features: features(enabled: true))
        defer { withExtendedLifetime(terminal) {} }
        #expect(GhosttyRuntime.shared.cursorBlinkTimeout == 30)
        #expect(reloads.count == 0)
    }

    @Test func theFeatureOffKeepsGhosttysBlink() async throws {
        let (settings, url) = try await settings(#"{"terminal": {"cursorBlinkTimeout": 30}}"#)
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        defer { GhosttyRuntime.shared.cursorBlinkTimeout = nil }
        GhosttyRuntime.shared.cursorBlinkTimeout = 5
        let (terminal, reloads) = primeThenFollow(settings, features: features(enabled: false))
        defer { withExtendedLifetime(terminal) {} }
        #expect(GhosttyRuntime.shared.cursorBlinkTimeout == nil)
        #expect(reloads.count == 0)
    }
}
