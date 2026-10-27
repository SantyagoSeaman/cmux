import CmuxNextDaemon
import Foundation
import Testing
@testable import CmuxNextAgentPane

/// Shell mode in the daemon (`appChildrenInDaemon`, p01-process-239): with the local daemon's app children a run is
/// a daemon job, so it outlives an app crash; a read of a job this app did not start (the page from before a
/// relaunch) goes to the daemon; Stop interrupts and closing the pane hangs the job up there. An older daemon keeps
/// the run this app's own child.
@MainActor @Suite(.serialized)
struct AgentPaneShellDaemonTests {
    @Test func aRunIsADaemonJobWhenTheDaemonServesAppChildren() async throws {
        let children = FakeShellChildren()
        let shell = Self.shell(children)
        let folder = NSTemporaryDirectory()
        let id = try await shell.run("echo hi", cwd: folder)
        let spec = try #require(await children.starts.first)
        #expect(id == "job-1")
        #expect(spec.executable.path == "/bin/sh")
        #expect(spec.arguments == ["-l", "-c", "echo hi"])
        #expect(spec.workingDirectory?.standardizedFileURL.path == URL(fileURLWithPath: folder).standardizedFileURL.path)
        #expect(spec.environment["TERM"] == "dumb", "the job gets shell mode's environment")
        #expect(!spec.lifeline, "a shell-mode command reads /dev/null")
        #expect(shell.hasRuns)
    }

    @Test func aReadOfAJobFromBeforeTheRelaunchGoesToTheDaemon() async throws {
        let children = FakeShellChildren()
        await children.answer("old", AppChildChunk(output: "still running\n", next: 14, truncated: true, exit: .init(code: 3)))
        let shell = Self.shell(children)
        let chunk = try await shell.read("old", after: 0)
        #expect(chunk == AgentPaneShell.Chunk(output: "still running\n", next: 14, truncated: true, exit: AgentPaneShell.Exit(code: 3)))
        await #expect(throws: AgentPaneShell.Failure.unknownRun) { try await shell.read("missing", after: 0) }
    }

    @Test func stopInterruptsAndClosingThePaneHangsUp() async throws {
        let children = FakeShellChildren()
        let shell = Self.shell(children)
        let first = try await shell.run("sleep 60", cwd: nil)
        let second = try await shell.run("sleep 60", cwd: nil)
        shell.stop(first)
        await children.waitForStops(1)
        shell.terminateAll()
        await children.waitForStops(3)
        let stops = await children.stops
        #expect(stops.contains(FakeShellChildren.Stop(child: first, mode: .interrupt)))
        #expect(stops.contains(FakeShellChildren.Stop(child: first, mode: .hangup)))
        #expect(stops.contains(FakeShellChildren.Stop(child: second, mode: .hangup)))
    }

    @Test func endedJobsFreeTheirRunningSlots() async throws {
        let children = FakeShellChildren()
        let shell = Self.shell(children)
        var ids: [String] = []
        for _ in 0..<AgentPaneShell.maximumRunning { ids.append(try await shell.run("sleep 60", cwd: nil)) }
        await #expect(throws: AgentPaneShell.Failure.tooMany) { try await shell.run("true", cwd: nil) }
        await children.end(ids[0])
        // The end reaches the pane as one main-thread task; the next run may start once it ran.
        var started: String?
        let deadline = ContinuousClock.now + .seconds(5)
        while started == nil, ContinuousClock.now < deadline {
            started = try? await shell.run("true", cwd: nil)
            if started == nil { try await Task.sleep(for: .milliseconds(10)) }
        }
        #expect(started != nil, "an ended daemon job still held its slot")
    }

    @Test func anOlderDaemonKeepsTheRunLocal() async throws {
        let children = FakeShellChildren(served: false)
        let shell = Self.shell(children)
        defer { shell.terminateAll() }
        let id = try await shell.run("exit 5", cwd: nil)
        var chunk = try await shell.read(id, after: 0)
        while chunk.exit == nil { chunk = try await shell.read(id, after: chunk.next) }
        #expect(chunk.exit == AgentPaneShell.Exit(code: 5))
        #expect(await children.starts.isEmpty)
    }

    @Test func theDaemonsRefusalsAreShellModes() {
        func refusal(_ code: String) -> AgentPaneShell.Failure {
            AgentPaneShell.failure(DaemonError.command(cmd: "app-child-start", message: "", code: code))
        }
        #expect(refusal("app-child.limit") == .tooMany)
        #expect(refusal("app-child.folder-missing") == .folderMissing)
        #expect(refusal("app-child.unknown") == .unknownRun)
        #expect(AgentPaneShell.failure(DaemonError.notConnected) == .spawnFailed(EIO))
    }

    private static func shell(_ children: FakeShellChildren) -> AgentPaneShell {
        let shell = AgentPaneShell(shell: "/bin/sh", environment: ["PATH": "/usr/bin:/bin"], home: NSTemporaryDirectory())
        shell.daemonChildren = { children }
        return shell
    }
}

/// The daemon's app children as shell mode sees them: starts and stops are recorded, reads answer from `chunks`,
/// and a job ends when the test calls ``end(_:)``.
actor FakeShellChildren: AppChildServing {
    struct Stop: Equatable {
        var child: String
        var mode: AppChildStopMode
    }

    private let served: Bool
    private(set) var starts: [AppChildSpec] = []
    private(set) var stops: [Stop] = []
    private var chunks: [String: AppChildChunk] = [:]
    private var exits: [String: AsyncStream<Void>.Continuation] = [:]
    private var stopWaiters: [(count: Int, continuation: AsyncStream<Void>.Continuation)] = []

    init(served: Bool = true) {
        self.served = served
    }

    var isServed: Bool { served }

    func answer(_ child: String, _ chunk: AppChildChunk) {
        chunks[child] = chunk
    }

    func end(_ child: String) {
        exits.removeValue(forKey: child)?.finish()
    }

    func waitForStops(_ count: Int) async {
        guard stops.count < count else { return }
        let (stream, continuation) = AsyncStream<Void>.makeStream()
        stopWaiters.append((count, continuation))
        for await _ in stream {}
    }

    func start(_ spec: AppChildSpec) async throws -> AppChildStarted {
        starts.append(spec)
        return AppChildStarted(child: "job-\(starts.count)", pid: 1)
    }

    func read(_ child: String, after: UInt64, wait: Duration) async throws -> AppChildChunk {
        guard let chunk = chunks[child] else { throw DaemonError.command(cmd: "app-child-read", message: "", code: "app-child.unknown") }
        return chunk
    }

    func exit(of child: String) async throws -> AppChildChunk.Exit? {
        let (stream, continuation) = AsyncStream<Void>.makeStream()
        exits[child] = continuation
        for await _ in stream {}
        return AppChildChunk.Exit(code: 0)
    }

    func stop(_ child: String, mode: AppChildStopMode) async throws {
        stops.append(Stop(child: child, mode: mode))
        for waiter in stopWaiters where stops.count >= waiter.count { waiter.continuation.finish() }
        stopWaiters.removeAll { stops.count >= $0.count }
    }

    func list(owner: String?) async throws -> [AppChildRow] { [] }
}
