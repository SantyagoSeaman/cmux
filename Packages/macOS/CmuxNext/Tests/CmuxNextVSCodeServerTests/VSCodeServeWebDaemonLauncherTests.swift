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
}
