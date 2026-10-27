public import CmuxNextTrace

extension DaemonConnectionConfiguration {
    /// A remote machine's control connection (`DaemonService.start(remote:)`): Cloud through its link socket, SSH
    /// through its forwarded socket, a server through its `cmux link dial` bridge. It mirrors the machine's session
    /// events and echoes the sidebar's placement shift like the local one. With `terminalStreams` the machine's
    /// terminal views are client-minted streams on it (`attach-streams-v1`, p05-ipc-151), as they are on the local
    /// connection (p05-ipc-095): a view adds no connection, no handshake round trip over the link and no bridge
    /// child. Streams are JSON lines, so every bridge carries them (``WireNegotiation``); a daemon that does not
    /// offer them keeps each view on a connection of its own through the same endpoint.
    public static func remote(retryWake: RetryWake, echoesPlacementShift: Bool, hangWatch: (any HangWatchScoping)?,
                              terminalStreams: TerminalStreamsDevice?) -> DaemonConnectionConfiguration {
        var configuration = DaemonConnectionConfiguration(retryWake: retryWake, terminalEnvironment: nil, sessionEvents: true,
                                                          terminalStreams: terminalStreams)
        configuration.echoesPlacementShift = echoesPlacementShift
        configuration.hangWatch = hangWatch
        return configuration
    }
}
