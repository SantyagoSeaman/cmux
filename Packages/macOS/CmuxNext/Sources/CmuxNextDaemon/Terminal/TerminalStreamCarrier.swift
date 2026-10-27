import Foundation

/// A daemon connection that carries terminal views as client streams
/// (`attach-streams-v1`, plans/cmux-next/app-daemon-connection-DESIGN.md): every
/// view attaches with one `attach-surface` on it and sends its commands on it.
///
/// Get one from ``DaemonConnection/terminalStreams(for:)``: the machine's
/// connection itself (no connection or handshake per view), or, while
/// `daemonTerminalStreams` is off or for a daemon without `attach-streams-v1`,
/// a connection of the view's own that carries only its stream
/// (``ownsConnection``). A shared carrier is valid for that connection's current
/// transport only: when the transport ends, every stream on it ends with
/// `connectionLost` and views reattach on the next one.
public struct TerminalStreamCarrier: Sendable {
    let transport: LineTransport
    let identity: DaemonIdentity
    /// The connection negotiated binary frames (`attach-frames-v1`).
    let frames: Bool
    /// The connection is the one view's: it closes when that view's stream ends.
    let ownsConnection: Bool
    /// The daemon serves `attach-streams-v1`: requests and events name the stream.
    /// False only on a connection of the view's own (`daemonTerminalStreams` off,
    /// or an older daemon), whose untagged events all belong to that one view
    /// (``LineTransport/deliverToOpenStreams(_:)``).
    let tagsStreams: Bool
}
