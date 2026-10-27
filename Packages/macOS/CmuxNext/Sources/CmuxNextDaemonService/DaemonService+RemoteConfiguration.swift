import CmuxNextDaemon

extension DaemonService {
    /// The configuration `start(remote:)` connects a remote machine's daemon with: its retry wake, the
    /// placement-shift echo, the hang watch and, while feature `terminalStreams` gave this service a device
    /// (``terminalStreams``, handed by `MachineRegistry`), its terminal views as streams on the one bridged
    /// connection with this Mac as the shared-sizing device (p05-ipc-151).
    var remoteConfiguration: DaemonConnection.Configuration {
        .remote(retryWake: retryWake, echoesPlacementShift: echoesPlacementShift, hangWatch: hangWatch,
                terminalStreams: terminalStreams)
    }
}
