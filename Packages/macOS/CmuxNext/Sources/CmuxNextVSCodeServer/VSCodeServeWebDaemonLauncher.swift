import CmuxNextCompat
public import CmuxNextDaemon
public import Foundation

/// Runs `serve-web` as a child of the local daemon (`appChildrenInDaemon`, p01-process-239), so the
/// server, and the URL with its connection token, outlive an app crash: the relaunched app's first
/// request adopts the running server instead of starting another. The daemon reads the server's
/// output and answers the start at its `Web UI available at` line; it ends the server when the app
/// quits or does not come back within its grace. A start that comes before the daemon's first connection (a
/// VS Code tab restored at launch) waits for it (``AppChildSlot/current()``), so it adopts the server a crashed app
/// left instead of starting a second one with a new URL. With no daemon children (the feature off, an older
/// daemon, a daemon that could not be reached) the launch goes to `fallback`, this app's own child.
public final class VSCodeServeWebDaemonLauncher: VSCodeServeWebLaunching {
    /// The daemon's tag for the server (`app-child-list`).
    public static let owner = "vscode-serve-web"
    static let readyText = "Web UI available at "

    private let children: AppChildSlot
    private let fallback: any VSCodeServeWebLaunching
    private let startupTimeout: Duration
    /// Only this app's first start adopts a server: a later one (Restart) must never take back the one it stops.
    private let adopts = Mutex(true)

    /// - Parameters:
    ///   - children: The local daemon's app children (set by the composition root when the feature is on).
    ///   - fallback: Starts the server as this app's child when the daemon cannot.
    ///   - startupTimeout: How long a server may take to print its URL (a first run downloads it).
    public init(children: AppChildSlot, fallback: any VSCodeServeWebLaunching, startupTimeout: Duration = .seconds(60)) {
        self.children = children
        self.fallback = fallback
        self.startupTimeout = startupTimeout
    }

    /// Starts `launch` in the daemon, or adopts the server a crashed app left there; see
    /// ``VSCodeServeWebLaunching/start(_:)``.
    public func start(_ launch: VSCodeServeWebLaunch) async throws -> VSCodeServeWebStarted {
        guard let daemon = await children.current(), await daemon.isServed else { return try await fallback.start(launch) }
        let firstStart = adopts.withLock { adopts in
            defer { adopts = false }
            return adopts
        }
        if firstStart, let adopted = await Self.running(in: daemon) { return adopted }
        let spec = AppChildSpec(name: "vscode-serve-web", owner: Self.owner, executable: launch.executableURL, arguments: launch.arguments,
                                environment: launch.environment, ready: Self.readyText, readyTimeout: startupTimeout)
        let started: AppChildStarted
        do {
            started = try await daemon.start(spec)
        } catch {
            throw Self.error(error)
        }
        let handle = DaemonServeWebProcess(child: started.child, children: daemon)
        guard !Task.isCancelled, let url = started.readyLine.flatMap(URL.init(vscodeServeWebOutput:)) else {
            handle.terminate()
            throw Task.isCancelled ? VSCodeServeWebError.stopped : VSCodeServeWebError.exitedBeforeReady
        }
        return VSCodeServeWebStarted(handle: handle, url: url)
    }

    /// The running server this app started before it relaunched, with the URL it printed.
    static func running(in children: any AppChildServing) async -> VSCodeServeWebStarted? {
        guard let rows = try? await children.list(owner: owner),
              let row = rows.last(where: { $0.running && $0.readyLine != nil }),
              let url = row.readyLine.flatMap(URL.init(vscodeServeWebOutput:)) else { return nil }
        return VSCodeServeWebStarted(handle: DaemonServeWebProcess(child: row.child, children: children), url: url)
    }

    /// The daemon's refusal as the service's error.
    static func error(_ error: any Error) -> VSCodeServeWebError {
        if error is CancellationError { return .stopped }
        guard case DaemonError.command(_, let message, let code, _, _) = error else { return .launchFailed(String(describing: error)) }
        switch code {
        case "app-child.not-ready": return message.contains("no ready line within") ? .startupTimedOut : .exitedBeforeReady
        default: return .launchFailed(message)
        }
    }
}
