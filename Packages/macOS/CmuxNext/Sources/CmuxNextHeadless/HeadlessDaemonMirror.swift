import CmuxNextDaemon

/// The headless process's daemon mirror: the first connect with the app's capped backoff (``DaemonStartup``), then
/// the store's mirror (``DaemonStore/run(connection:scheduler:)``) until the connection ends for good; meanwhile the
/// connection reconnects by itself. The app's DaemonService runs the same two steps for its home daemon.
struct HeadlessDaemonMirror: Sendable {
    let endpoint: DaemonEndpoint
    let store: DaemonStore

    /// Starts the mirror off the main actor (the store's methods hop to it); `connected` gets the connection once the
    /// first connect succeeded, before the mirror runs. The caller keeps the handle and cancels it to stop.
    func start(connected: @escaping @Sendable (DaemonConnection) async -> Void) -> Task<Void, Never> {
        let (endpoint, store) = (endpoint, store)
        // task-owner: HeadlessServices keeps the handle (`mirror`) and cancels it in stop(); the store's run ends when
        // stop() closes the connection
        return Task {
            let first = await DaemonStartup.shared.connect(makeConnection: { DaemonConnection(endpoint: endpoint) },
                                                           onFailure: { error in await store.markFailed(error.description) })
            guard let (connection, identity) = first else { return }
            guard !Task.isCancelled else { return await connection.close() }
            await connected(connection)
            await store.noteHandshake(identity)
            await store.run(connection: connection)
            await connection.close()
        }
    }
}
