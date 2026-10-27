public import CmuxNextDaemon
import Darwin
import Foundation

/// Shell mode in the daemon (`appChildrenInDaemon`, p01-process-239): a run is a daemon job
/// (`app-child-*`) instead of this app's child, so a command keeps running, and its output keeps
/// coming, when the app crashes; the relaunched page reads it again by id. The daemon reads the
/// output, reaps the command and parks a read until there is news, so nothing here waits on a thread.
extension AgentPaneShell {
    /// One daemon job of this pane: whether it still runs.
    final class DaemonRun {
        var running = true
    }

    /// Starts `command` in `folder` as a daemon job; the caller holds the spawn slot.
    func runInDaemon(_ command: String, folder: String, children: any AppChildServing) async throws(Failure) -> String {
        let spec = AppChildSpec(name: "agent-pane-shell", owner: daemonOwner, executable: URL(fileURLWithPath: shell),
                                arguments: ["-l", "-c", command], environment: environment,
                                workingDirectory: URL(fileURLWithPath: folder, isDirectory: true))
        let started: AppChildStarted
        do {
            started = try await children.start(spec)
        } catch {
            throw Self.failure(error)
        }
        let id = started.child
        track(id, children: children)
        return id
    }

    /// The daemon job's output after `after`; a job this pane did not start (the app relaunched since)
    /// is read the same way, and its first answer while it runs makes it this pane's job again, so Stop,
    /// closing the pane and the running cap cover it as they cover the jobs it started.
    func readInDaemon(_ id: String, after: Int, children: any AppChildServing) async throws(Failure) -> Chunk {
        let chunk: AppChildChunk
        do {
            chunk = try await children.read(id, after: UInt64(clamping: after), wait: Self.readWait)
        } catch {
            throw Self.failure(error)
        }
        if daemonRuns[id] == nil, chunk.exit == nil { track(id, children: children) }
        return Chunk(output: chunk.output, next: Int(clamping: chunk.next), truncated: chunk.truncated,
                     exit: chunk.exit.map { Exit(code: $0.code, signal: $0.signal) })
    }

    /// Makes daemon job `id` this pane's: its end is watched, and a pane that closed meanwhile hangs it up as
    /// terminateAll() did the others.
    private func track(_ id: String, children: any AppChildServing) {
        let run = DaemonRun()
        daemonRuns[id] = run
        daemonOrder.append(id)
        evictDaemonRuns()
        watchExit(of: id, run: run, children: children)
        if terminated { stopInDaemon(id, mode: .hangup, children: children) }
    }

    /// Stops a daemon job; the daemon escalates `interrupt` to SIGTERM and SIGKILL as ``stop(_:)`` does.
    func stopInDaemon(_ id: String, mode: AppChildStopMode, children: any AppChildServing) {
        // task-owner: one stop request; it ends with the daemon's answer.
        Task { @concurrent in try? await children.stop(id, mode: mode) }
    }

    /// Marks `run` ended when the daemon says so: an exit event (`until_exit`), not a poll.
    private func watchExit(of id: String, run: DaemonRun, children: any AppChildServing) {
        let mainWork = deadlines.work
        // task-owner: waits for one daemon job's end; ends with the job or the daemon connection.
        Task { @concurrent in
            _ = try? await children.exit(of: id)
            mainWork.postFromAnyThread(.visible, owner: "AgentPaneShell.daemonRunEnded") { run.running = false }
        }
    }

    /// Drops the oldest ended jobs past ``maximumKept`` (the daemon keeps their output on its own bound).
    private func evictDaemonRuns() {
        while daemonOrder.count > Self.maximumKept, let index = daemonOrder.firstIndex(where: { daemonRuns[$0]?.running != true }) {
            daemonRuns.removeValue(forKey: daemonOrder.remove(at: index))
        }
    }

    /// The daemon's refusal as shell mode's.
    static func failure(_ error: any Error) -> Failure {
        guard case DaemonError.command(_, _, let code, _, _) = error else { return .spawnFailed(EIO) }
        switch code {
        case "app-child.limit": return .tooMany
        case "app-child.folder-missing": return .folderMissing
        case "app-child.unknown": return .unknownRun
        default: return .spawnFailed(EIO)
        }
    }
}
