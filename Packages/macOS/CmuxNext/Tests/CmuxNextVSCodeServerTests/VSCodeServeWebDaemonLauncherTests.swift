import CmuxNextDaemon
import Foundation
import Testing
@testable import CmuxNextVSCodeServer

/// `serve-web` in the daemon (`appChildrenInDaemon`, p01-process-239): the server starts as a daemon child that
/// answers at its Web UI line, a relaunched app adopts the server a crashed app left (the same URL and token),
/// and with no daemon children the app's own launcher runs it.
@Suite struct VSCodeServeWebDaemonLauncherTests {
    static let launch = VSCodeServeWebLaunch(executableURL: URL(fileURLWithPath: "/Applications/Code.app/bin/code-tunnel"),
                                             arguments: ["serve-web", "--connection-token-file", "/tmp/token"],
                                             environment: ["HOME": "/Users/me"])

    @Test func theServerStartsInTheDaemonAndAnswersAtItsWebUILine() async throws {
        let children = FakeServeWebChildren()
        await children.setReadyLine("[info] Web UI available at http://127.0.0.1:8123?tkn=abc")
        let launcher = VSCodeServeWebDaemonLauncher(children: Self.slot(children), fallback: FakeServeWebLauncher())
        let started = try await launcher.start(Self.launch)
        #expect(started.url.absoluteString == "http://127.0.0.1:8123?tkn=abc")
        let spec = try #require(await children.starts.first)
        #expect(spec.owner == VSCodeServeWebDaemonLauncher.owner)
        #expect(spec.ready == "Web UI available at ")
        #expect(spec.executable == Self.launch.executableURL)
        #expect(spec.arguments == Self.launch.arguments)
        #expect(spec.environment == Self.launch.environment)
        #expect(started.handle.isRunning)
        started.handle.terminate()
        #expect(!started.handle.isRunning)
        #expect(await children.waitForStop() == "server-1")
    }

    @Test func aRelaunchedAppAdoptsTheServerItLeftWithItsURL() async throws {
        let children = FakeServeWebChildren()
        await children.setRows([
            AppChildRow(child: "old", pid: 7, name: "vscode-serve-web", owner: VSCodeServeWebDaemonLauncher.owner, running: true,
                        readyLine: "Web UI available at http://127.0.0.1:9000?tkn=kept"),
        ])
        let launcher = VSCodeServeWebDaemonLauncher(children: Self.slot(children), fallback: FakeServeWebLauncher())
        let started = try await launcher.start(Self.launch)
        #expect(started.url.absoluteString == "http://127.0.0.1:9000?tkn=kept")
        #expect(await children.starts.isEmpty, "a second server started beside the running one")
    }

    /// A VS Code tab restored at launch asks for the URL before the daemon's handshake: the start waits for the
    /// connection instead of taking it for the feature off and starting a second, in-app server with a new URL.
    @MainActor @Test func aStartBeforeTheDaemonConnectsWaitsForTheDaemon() async throws {
        let children = FakeServeWebChildren()
        await children.setReadyLine("Web UI available at http://127.0.0.1:8123?tkn=abc")
        let daemon = LateDaemon()
        let slot = AppChildSlot()
        slot.resolve(with: { daemon.children }, firstConnection: { await daemon.connected() })
        let fallback = FakeServeWebLauncher()
        let launcher = VSCodeServeWebDaemonLauncher(children: slot, fallback: fallback)
        let start = Task { try await launcher.start(Self.launch) }
        await daemon.waitUntilAsked()
        daemon.connect(children)
        let started = try await start.value
        #expect(started.url.absoluteString == "http://127.0.0.1:8123?tkn=abc")
        #expect(await fallback.launches.isEmpty, "the start ran the server in the app before the daemon connected")
        #expect(await children.starts.count == 1)
    }

    /// A clean quit ends the app's children in the daemon at once (`app-child-release-all`), and never waits for a
    /// first connection that an app which never connected will not get.
    @MainActor @Test func aCleanQuitReleasesTheDaemonChildren() async {
        let children = FakeServeWebChildren()
        await Self.slot(children).releaseAllForQuit()
        #expect(await children.releases == 1)
        let never = LateDaemon()
        let unconnected = AppChildSlot()
        unconnected.resolve(with: { never.children }, firstConnection: { await never.connected() })
        await unconnected.releaseAllForQuit()  // returns: nothing to release, and no wait for the handshake
        await AppChildSlot().releaseAllForQuit()  // the feature off
    }

    /// A failed exit watch is not the server's end: only the daemon's "no such child" or its own end says so.
    @Test func onlyADefiniteAnswerEndsTheAdoptedServer() {
        #expect(DaemonServeWebProcess.watchSaysEnded(DaemonError.command(cmd: "app-child-read", message: "", code: "app-child.unknown")))
        #expect(DaemonServeWebProcess.watchSaysEnded(DaemonError.daemonShutdown))
        #expect(!DaemonServeWebProcess.watchSaysEnded(DaemonError.command(cmd: "app-child-read", message: "", code: "app-child.limit")))
        #expect(!DaemonServeWebProcess.watchSaysEnded(DaemonError.connectionClosed(reason: "eof")))
        #expect(!DaemonServeWebProcess.watchSaysEnded(CancellationError()))
    }

    @Test func withNoDaemonChildrenTheAppRunsTheServer() async throws {
        let fallback = FakeServeWebLauncher()
        let launcher = VSCodeServeWebDaemonLauncher(children: AppChildSlot(), fallback: fallback)
        _ = try await launcher.start(Self.launch)
        #expect(await fallback.launches == [Self.launch])
    }

    @Test func theDaemonsRefusalsAreTheServicesErrors() {
        func error(_ message: String) -> VSCodeServeWebError {
            VSCodeServeWebDaemonLauncher.error(DaemonError.command(cmd: "app-child-start", message: message, code: "app-child.not-ready"))
        }
        #expect(error("the child did not become ready: no ready line within 60 s") == .startupTimedOut)
        #expect(error("the child did not become ready: it exited first") == .exitedBeforeReady)
        #expect(VSCodeServeWebDaemonLauncher.error(CancellationError()) == .stopped)
    }

    private static func slot(_ children: FakeServeWebChildren) -> AppChildSlot {
        let slot = AppChildSlot()
        slot.resolve { children }
        return slot
    }
}

/// The daemon's app children as the `serve-web` launcher sees them.
actor FakeServeWebChildren: AppChildServing {
    private(set) var starts: [AppChildSpec] = []
    private var rows: [AppChildRow] = []
    private var readyLine: String?
    private var stopped: [String] = []
    private var stopWaiter: AsyncStream<Void>.Continuation?
    private var exits: [AsyncStream<Void>.Continuation] = []

    var isServed: Bool { true }

    func setReadyLine(_ line: String) { readyLine = line }

    func setRows(_ rows: [AppChildRow]) { self.rows = rows }

    func waitForStop() async -> String? {
        if stopped.isEmpty {
            let (stream, continuation) = AsyncStream<Void>.makeStream()
            stopWaiter = continuation
            for await _ in stream {}
        }
        return stopped.first
    }

    func start(_ spec: AppChildSpec) async throws -> AppChildStarted {
        starts.append(spec)
        return AppChildStarted(child: "server-\(starts.count)", pid: 1, readyLine: readyLine)
    }

    func read(_ child: String, after: UInt64, wait: Duration) async throws -> AppChildChunk {
        AppChildChunk(output: "", next: after)
    }

    func exit(of child: String) async throws -> AppChildChunk.Exit? {
        // The server runs until the test ends: the continuation is kept, so the stream never finishes.
        let (stream, continuation) = AsyncStream<Void>.makeStream()
        exits.append(continuation)
        for await _ in stream {}
        return nil
    }

    func stop(_ child: String, mode: AppChildStopMode) async throws {
        stopped.append(child)
        stopWaiter?.finish()
        stopWaiter = nil
    }

    func list(owner: String?) async throws -> [AppChildRow] {
        rows.filter { owner == nil || $0.owner == owner }
    }

    private(set) var releases = 0

    func releaseAll() async throws -> Int {
        releases += 1
        return 1
    }
}

/// The local daemon as the slot sees it at launch: not connected until the test calls ``connect(_:)``.
@MainActor final class LateDaemon {
    private(set) var children: (any AppChildServing)?
    private let connection: AsyncStream<Void>
    private let connectionContinuation: AsyncStream<Void>.Continuation
    private let asked: AsyncStream<Void>
    private let askedContinuation: AsyncStream<Void>.Continuation

    init() {
        (connection, connectionContinuation) = AsyncStream<Void>.makeStream()
        (asked, askedContinuation) = AsyncStream<Void>.makeStream()
    }

    /// The slot's first-connection wait: returns once ``connect(_:)`` ran.
    func connected() async {
        askedContinuation.yield()
        for await _ in connection {}
    }

    /// Returns once a use is waiting for the connection.
    func waitUntilAsked() async {
        for await _ in asked { return }
    }

    func connect(_ children: any AppChildServing) {
        self.children = children
        connectionContinuation.finish()
    }
}
