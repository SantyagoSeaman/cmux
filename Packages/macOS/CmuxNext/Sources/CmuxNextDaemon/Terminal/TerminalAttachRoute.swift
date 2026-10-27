public import CmuxNextTrace
public import Foundation

/// Where a terminal view attaches: the carrier of its stream
/// (``TerminalStreamCarrier``). Every view is a stream; only the carrier differs.
///
/// A machine's `DaemonService` gives the route of its views
/// (`DaemonService.terminalAttachRoute`);
/// ``TerminalAttachment/attach(route:target:size:claimGeometry:snapshotVersion:localHistory:images:device:hangs:createIntent:inputQueue:clientName:)``
/// takes the carrier it returns.
///
/// ```swift
/// // A test view that never reaches a daemon.
/// let route = TerminalAttachRoute { _ in throw CancellationError() }
/// ```
public struct TerminalAttachRoute: Sendable {
    /// What a view brings to its carrier. A shared carrier declared the
    /// device once per connect; a connection of the view's own uses all of it.
    public struct View: Sendable {
        /// The view's device for shared sizing; nil joins none.
        public var device: TerminalSizingDevice?
        /// Watches a connection of the view's own (its reader's lines); a shared
        /// connection's lines are its machine connection's.
        public var hangs: (any HangWatchScoping)?
        /// The view's serial input queue: a connection of its own drains its socket
        /// writer there (nil: its own queue).
        public var inputQueue: DispatchQueue?
        /// The `set-client-info` name of a connection of the view's own.
        public var clientName: String

        /// Makes a view's request for a carrier.
        ///
        /// - Parameters:
        ///   - device: The view's device for shared sizing, or nil.
        ///   - hangs: Watches a connection of the view's own, or nil.
        ///   - inputQueue: The view's serial input queue, or nil.
        ///   - clientName: The `set-client-info` name of a connection of the view's own.
        public init(device: TerminalSizingDevice?, hangs: (any HangWatchScoping)? = nil,
                    inputQueue: DispatchQueue? = nil, clientName: String = "cmux-next-terminal") {
            self.device = device
            self.hangs = hangs
            self.inputQueue = inputQueue
            self.clientName = clientName
        }
    }

    /// The carrier of a view's stream.
    public let carrier: @Sendable (View) async throws -> TerminalStreamCarrier

    /// Makes a route.
    ///
    /// - Parameter carrier: Returns the carrier of a view's stream
    ///   (`DaemonConnection.terminalStreams(for:)`).
    public init(carrier: @escaping @Sendable (View) async throws -> TerminalStreamCarrier) {
        self.carrier = carrier
    }
}
