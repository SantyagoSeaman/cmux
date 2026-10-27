import CmuxNextDaemon
@testable import CmuxNextHeadless
import Foundation
import Testing

/// The headless daemon mirror's first connect (p15-oracle-160): it keeps trying while the owner is not up, and an
/// owner that comes up after the timed retry budget still wakes it (the socket watch the app's DaemonService arms).
/// Run: `swift test --filter HeadlessDaemonMirrorTests`.
@MainActor @Suite struct HeadlessDaemonMirrorTests {
    @Test func onlyAPermanentFailureEndsConnecting() async {
        let store = DaemonStore()
        let mirror = HeadlessDaemonMirror(endpoint: DaemonEndpoint(socketPath: "/tmp/hl-none/d.sock"), store: store)

        // The owner is not up yet: the startup retries, so the topology must not say `failed`.
        await mirror.noteFailure(.connectFailed(path: "/tmp/hl-none/d.sock", errno: ENOENT))
        #expect(store.connectionState == .connecting)
        await mirror.noteFailure(.timedOut("first connection to cmux-tui"))
        #expect(store.connectionState == .connecting)

        // No retry fixes a socket another app serves.
        await mirror.noteFailure(.wrongApp("nginx"))
        #expect(store.connectionState == .failed(DaemonError.wrongApp("nginx").description))
    }

    @Test func theFirstConnectWatchesTheOwnersSocket() async throws {
        let directory = URL(fileURLWithPath: "/tmp", isDirectory: true).appending(path: "hl-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let wake = RetryWake(owner: "HeadlessDaemonMirrorTests")
        let mirror = HeadlessDaemonMirror(endpoint: DaemonEndpoint(socketPath: directory.appending(path: "d.sock").path),
                                          store: DaemonStore(), wake: wake)

        let task = mirror.start { _ in }
        // Past the timed budget the startup waits for this wake only: the missing socket is watched, so its creation
        // wakes it (a wake with no watch says `unwatched` and parks the loop for good).
        #expect(wake.rebaseline() == .missing)
        task.cancel()
        await task.value
    }
}
