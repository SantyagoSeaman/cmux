public import Foundation
import CmuxNextCompat

/// Browser traffic to one machine's loopback services (`loopback-forward-v1`,
/// plans/cmux-next/remote-localhost.md) as streams on the machine's control
/// connection (p05-ipc-101, design 3.1): no connection of its own. A stream's
/// lines go out at bulk priority and its events are decoded off the reader
/// thread (``LoopbackStreamTable``), so forwarded bytes never delay a reply,
/// an event or a key; the daemon writes each stream on its bulk lane.
///
/// A stream ends with the connection that carries it (never a fallback to this
/// Mac); the next `open` rides the connection's next epoch. Nothing is sent
/// while no stream is wanted.
public actor LoopbackForwardClient {
    public typealias EndpointProvider = @Sendable () async throws -> DaemonEndpoint
    /// The machine's control connection (`DaemonService.connection`), once it exists.
    public typealias ConnectionProvider = @Sendable () async throws -> DaemonConnection

    /// This client's receive window per stream.
    public static let receiveWindow = 256 * 1024
    public static let capability = DaemonCapability.loopbackForward.rawValue

    /// Where the streams travel.
    private enum Source: Sendable {
        /// The machine's control connection.
        case control(ConnectionProvider)
        /// A connection of this client's own to a socket no ``DaemonConnection`` serves: the
        /// `debug.remote-localhost` socket and tests.
        case dedicated(EndpointProvider)
    }

    /// The connection epoch the streams ride now.
    private struct Carrier: Sendable {
        let transport: LineTransport
        let table: LoopbackStreamTable
        /// The daemon has `browser-runtime-v1` and this connection echoed it.
        let runtimeEnabled: Bool
        /// The transport is this client's (``Source/dedicated``): it closes with the client.
        let owned: Bool
    }

    private let source: Source
    let requestTimeout: Duration
    private let openTimeout: Duration
    private var carrier: Carrier?
    private var connecting: Task<Carrier, any Error>?
    /// Set by `close()`: a closed client never connects again, also for an
    /// open that captured it or a connect in flight (cx-bj1o review).
    private var isClosed = false
    /// The connected daemon has `browser-runtime-v1` and the connection opted in.
    var runtimeEnabled: Bool { carrier?.runtimeEnabled ?? false }

    /// Streams on the machine's control connection.
    public init(requestTimeout: Duration = .seconds(2), openTimeout: Duration = .seconds(5),
                connection: @escaping ConnectionProvider) {
        source = .control(connection)
        self.requestTimeout = requestTimeout
        self.openTimeout = openTimeout
    }

    /// Streams on a connection of this client's own to `endpoint`'s socket: only for a socket no
    /// ``DaemonConnection`` of the app serves (the `debug.remote-localhost` socket, tests).
    public init(requestTimeout: Duration = .seconds(2), openTimeout: Duration = .seconds(5),
                endpoint: @escaping EndpointProvider) {
        source = .dedicated(endpoint)
        self.requestTimeout = requestTimeout
        self.openTimeout = openTimeout
    }

    /// Opens a stream to `host:port` on the machine. `host` must be a
    /// loopback name or literal; the daemon refuses anything else.
    public func open(host: String, port: UInt16) async throws -> LoopbackStream {
        let carrier = try await liveCarrier()
        guard let stream = carrier.table.makeStream(owner: ObjectIdentifier(self), receiveWindow: Self.receiveWindow,
                                                    sender: TransportLineSender(transport: carrier.transport)) else {
            throw LoopbackForwardError.unavailable("the connection closed while opening")
        }
        let request = LoopbackOpenRequest(stream: stream.id, host: host, port: port, window: Self.receiveWindow)
        do {
            let response = try await DaemonConnection.perform(request, on: carrier.transport, timeout: openTimeout)
            // A connection that ended meanwhile ended the stream too: the window it adds is never spent.
            stream.opened(address: response.address, sendWindow: response.window)
        } catch {
            stream.abandon()
            if let error = error as? DaemonError { throw Self.map(error) }
            throw error
        }
        return stream
    }

    /// This client's open streams (diagnostics).
    public var openStreamCount: Int { carrier?.table.count(owner: ObjectIdentifier(self)) ?? 0 }

    /// Ends this client's streams. A connection of its own closes, and the browser runtimes it
    /// started end with it; on the control connection a runtime ends with `stopBrowserRuntime`
    /// or with that connection.
    public func close() {
        isClosed = true
        connecting?.cancel()
        connecting = nil
        guard let carrier else { return }
        self.carrier = nil
        if carrier.owned {
            carrier.transport.close()
        } else {
            for stream in carrier.table.removeAll(owner: ObjectIdentifier(self)) { stream.close() }
        }
    }

    // MARK: Private

    func liveTransport() async throws -> LineTransport {
        try await liveCarrier().transport
    }

    private func liveCarrier() async throws -> Carrier {
        if isClosed { throw LoopbackForwardError.unavailable("the forwarding connection is closed") }
        if let carrier, !carrier.transport.isClosed { return carrier }
        carrier = nil
        if let connecting { return try await connecting.value }
        let source = source
        let task = Task { try await Self.connect(source, requestTimeout: requestTimeout, readyWithin: openTimeout) }
        connecting = task
        defer { connecting = nil }
        let carrier = try await task.value
        guard !isClosed else {
            if carrier.owned { carrier.transport.close() }
            throw LoopbackForwardError.unavailable("the forwarding connection is closed")
        }
        self.carrier = carrier
        return carrier
    }

    private static func connect(_ source: Source, requestTimeout: Duration, readyWithin limit: Duration) async throws -> Carrier {
        switch source {
        case .control(let provider):
            let connection: DaemonConnection
            do {
                connection = try await provider()
            } catch {
                throw LoopbackForwardError.unavailable(String(describing: error))
            }
            return try await carrier(on: connection, within: limit)
        case .dedicated(let endpoint):
            return try await dedicatedCarrier(endpoint, requestTimeout: requestTimeout)
        }
    }

    /// The control connection's ready epoch, waited for up to `limit` (a first connect, a reconnect).
    private static func carrier(on connection: DaemonConnection, within limit: Duration) async throws -> Carrier {
        let echoed = Set(connection.configuration.handshakeCapabilities)
        var after: UInt64?
        // Twice at most: a ready epoch whose transport closed already has its close on the way to the
        // connection, which then reconnects; the wait is for the epoch after it.
        for _ in 0..<2 {
            guard await connection.untilReady(after: after, within: limit) else { break }
            guard let ready = await readyEpoch(connection) else { continue }
            guard ready.identity.supports(capability), echoed.contains(capability) else { throw LoopbackForwardError.unsupported }
            guard let table = ready.transport.loopbackStreams() else {
                after = ready.serial
                continue
            }
            let runtime = BrowserRuntimeRequest.capability
            return Carrier(transport: ready.transport, table: table,
                           runtimeEnabled: ready.identity.supports(runtime) && echoed.contains(runtime), owned: false)
        }
        throw LoopbackForwardError.unavailable("the machine's connection is not ready")
    }

    private static func readyEpoch(_ connection: isolated DaemonConnection) -> DaemonReadyConnection? {
        connection.ready
    }

    private static func dedicatedCarrier(_ endpoint: EndpointProvider, requestTimeout: Duration) async throws -> Carrier {
        let resolved: DaemonEndpoint
        do {
            resolved = try await endpoint()
        } catch {
            throw LoopbackForwardError.unavailable(String(describing: error))
        }
        let transport: LineTransport
        do {
            transport = try await LineTransport.open(path: resolved.socketPath, bridge: resolved.bridge, role: .loopbackForward,
                                                     stats: resolved.wireStats)
        } catch {
            throw LoopbackForwardError.unavailable(error.description)
        }
        // The streams' events reach the transport's table (``LineTransport/routeLoopback(event:line:)``) and its
        // close ends them; nothing else on this connection is read.
        transport.start(onEvent: { _, _, _ in }, onClose: { _ in })
        do {
            let identity = try await DaemonConnection.perform(IdentifyRequest(), on: transport, timeout: requestTimeout)
            guard identity.supports(capability) else {
                transport.close()
                throw LoopbackForwardError.unsupported
            }
            // Browser runtimes share this connection: they end with it, like its streams.
            let runtime = identity.supports(BrowserRuntimeRequest.capability)
            _ = try await DaemonConnection.perform(
                SetClientInfoRequest(name: "cmux-next loopback", kind: "loopback-forward",
                                     capabilities: [capability] + (runtime ? [BrowserRuntimeRequest.capability] : [])),
                on: transport, timeout: requestTimeout)
            guard let table = transport.loopbackStreams() else {
                throw LoopbackForwardError.unavailable("the connection closed while opening")
            }
            return Carrier(transport: transport, table: table, runtimeEnabled: runtime, owned: true)
        } catch let error as LoopbackForwardError {
            throw error
        } catch let error as DaemonError {
            transport.close()
            throw map(error)
        }
    }

    private static func map(_ error: DaemonError) -> LoopbackForwardError {
        switch error {
        case .command(_, let message, let code, _, _): .from(code: code, message: message)
        case .timedOut: .timedOut
        default: .unavailable(error.description)
        }
    }
}
