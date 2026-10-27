import CmuxNextCompat
import CmuxNextDaemon

/// A `serve-web` process that runs under the local daemon (``VSCodeServeWebDaemonLauncher``). It is
/// running until the daemon reports its end (an exit event, `until_exit`) or ``terminate()`` stops it. A watch
/// that fails without that answer (the connection dropped, the daemon refused the read) leaves it running: the
/// server is still there, and a fresh server would change the URL.
final class DaemonServeWebProcess: VSCodeServeWebProcessHandle {
    private let child: String
    private let children: any AppChildServing
    // Written by the exit watch and terminate(), read by the service's actor; held only to copy a Bool.
    private let running = Mutex(true)

    init(child: String, children: any AppChildServing) {
        self.child = child
        self.children = children
        // task-owner: waits for the server's end in the daemon; ends with the server or the connection.
        Task { @concurrent [weak self, children, child] in
            let ended: Bool
            do {
                _ = try await children.exit(of: child)
                ended = true
            } catch {
                ended = DaemonServeWebProcess.watchSaysEnded(error)
            }
            guard ended, let self else { return }
            self.running.withLock { $0 = false }
        }
    }

    var isRunning: Bool { running.withLock { $0 } }

    func terminate() {
        let wasRunning = running.withLock { running in
            defer { running = false }
            return running
        }
        guard wasRunning else { return }
        let (children, child) = (children, child)
        // task-owner: one stop request; it ends with the daemon's answer.
        Task { @concurrent in try? await children.stop(child, mode: .terminate) }
    }

    /// Whether a failed exit watch still proves the server ended: the daemon no longer knows the child, or the
    /// daemon itself ended (its end ends every app child). Anything else (a closed connection, a refused read)
    /// says nothing about the server.
    static func watchSaysEnded(_ error: any Error) -> Bool {
        switch error {
        case DaemonError.command(_, _, let code, _, _): code == "app-child.unknown"
        case DaemonError.daemonShutdown: true
        default: false
        }
    }
}
