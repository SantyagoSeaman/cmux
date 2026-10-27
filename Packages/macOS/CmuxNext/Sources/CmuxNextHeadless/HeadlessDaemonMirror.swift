import CmuxNextDaemon

/// The headless process's daemon mirror: the first connect with the app's capped backoff (``DaemonStartup``), then
/// the store's mirror (``DaemonStore/run(connection:scheduler:)``) until the connection ends for good; meanwhile the
/// connection reconnects by itself. The app's DaemonService runs the same two steps for its home daemon.
///
/// Past the timed retry budget the first connect waits for ``wake`` only. As the app's DaemonService does, the wake
/// watches the owner's socket and its ready marker (`<socket>.websocket.json`), so an owner that comes up late, or
/// restarts before the first handshake, wakes the loop; the connection then reconnects through the same wake.
struct HeadlessDaemonMirror: Sendable {
    let endpoint: DaemonEndpoint
    let store: DaemonStore
    let wake: RetryWake

    init(endpoint: DaemonEndpoint, store: DaemonStore, wake: RetryWake = RetryWake(owner: "HeadlessDaemonMirror")) {
        self.endpoint = endpoint
        self.store = store
        self.wake = wake
    }

    /// Starts the mirror off the main actor (the store's methods hop to it); `connected` gets the connection once the
    /// first connect succeeded, before the mirror runs. The caller keeps the handle and cancels it to stop.
    func start(connected: @escaping @Sendable (DaemonConnection) async -> Void) -> Task<Void, Never> {
        let socket = endpoint.socketPath
        wake.watch(file: socket, companions: [socket + ".websocket.json"])
        let (endpoint, wake, mirror) = (endpoint, wake, self)
        // task-owner: HeadlessServices keeps the handle (`mirror`) and cancels it in stop(); the store's run ends when
        // stop() closes the connection
        return Task {
            let first = await DaemonStartup.shared.connect(
                wake: wake,
                makeConnection: { DaemonConnection(endpoint: endpoint, configuration: DaemonConnection.Configuration(retryWake: wake)) },
                onFailure: { error in await mirror.noteFailure(error) }
            )
            guard let (connection, identity) = first else { return }
            guard !Task.isCancelled else { return await connection.close() }
            await connected(connection)
            await store.noteHandshake(identity)
            await store.run(connection: connection)
            await connection.close()
        }
    }

    /// A failed first-connect attempt. Only a failure no retry can fix (``DaemonStartup/isPermanent(_:)``) ends the
    /// store's `connecting` state; a transient one (the owner not up yet, a refused connect) leaves it connecting
    /// while the startup retries.
    func noteFailure(_ error: DaemonError) async {
        guard DaemonStartup.shared.isPermanent(error) else { return }
        await store.markFailed(error.description)
    }
}
