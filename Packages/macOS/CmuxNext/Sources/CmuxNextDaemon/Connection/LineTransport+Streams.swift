import Foundation

/// A `cmux.protocol/2` stream open that the daemon acknowledged: its `result`, the transport that
/// carries the stream (where ``LineTransport/cancelResourceStream(_:timeout:)`` goes), and that
/// connection's recorder, where parsing the stream's lines counts.
struct ResourceStreamOpened<R: Sendable>: Sendable {
    var result: R
    var transport: LineTransport
    var recorder: DaemonWireStats.Recorder?
}

/// A connection that carries client-minted `cmux.protocol/2` streams next to its requests
/// (plans/cmux-next/chrome-parity/p05-ipc.md 3.1): ``DaemonConnection`` on its current transport,
/// or a bare ``LineTransport`` (tests).
protocol ResourceStreamCarrier: Sendable {
    /// Routes `streamID` to `sink`, then sends `operation` with `params` (`stream_id` among them) and
    /// decodes the acknowledgement's `result`. `timeout` covers dispatch and the acknowledgement.
    func openResourceStream<R: Decodable & Sendable>(_ streamID: String, operation: DaemonCommandName,
                                                     params: [String: JSONValue], timeout: Duration, as type: R.Type,
                                                     sink: @escaping LineTransport.StreamSink) async throws -> ResourceStreamOpened<R>
}

extension LineTransport: ResourceStreamCarrier {
    /// What the reader hands a stream's sink, on the reader thread, in wire order.
    enum StreamDelivery: Sendable {
        /// One `stream_item` line, whole.
        case item(Data)
        /// The `stream_end` line; nothing follows it.
        case end(Data)
        /// The connection closed before the stream ended.
        case closed(TransportCloseReason)
    }

    /// Takes one stream's lines on the reader thread; it must not block (it hands them on).
    typealias StreamSink = @Sendable (StreamDelivery) -> Void

    /// Sends `streamID`'s lines to `sink` until its `stream_end` or the connection's close. They stay
    /// off the control sequence: they never reach `onEvent` and never count in a reply's event barrier,
    /// so a long read is independent of every other request and event on the connection. Call before
    /// the request that opens the stream. Nothing is routed once the connection closed.
    func routeStream(_ streamID: String, to sink: @escaping StreamSink) {
        state.withLock { state in
            guard state.closed == nil else { return }
            state.streamSinks[streamID] = sink
        }
    }

    /// Stops routing `streamID` here; its later lines are dropped as unrouted.
    func dropStream(_ streamID: String) {
        _ = state.withLock { $0.streamSinks.removeValue(forKey: streamID) }
    }

    /// Opens a stream on this connection (resource-api-v2.md, stream opens on a shared transport).
    /// An `ok:false` answer rejects the open conclusively, so the route goes with no cancel. Any other
    /// failure after the route (no acknowledgement in time, or one that does not decode) releases it
    /// with `stream.cancel`; the open's error is what the caller sees.
    func openResourceStream<R: Decodable & Sendable>(_ streamID: String, operation: DaemonCommandName,
                                                     params: [String: JSONValue], timeout: Duration, as type: R.Type,
                                                     sink: @escaping StreamSink) async throws -> ResourceStreamOpened<R> {
        routeStream(streamID, to: sink)
        do {
            let response = try await sendStreamRequest(operation, params: params, timeout: timeout)
            let result = try ResourceRequestEnvelope.decodeResult(R.self, from: response)
            return ResourceStreamOpened(result: result, transport: self, recorder: response.recorder)
        } catch {
            if let refused = error as? DaemonError, case .command = refused {
                dropStream(streamID)
            } else {
                await cancelResourceStream(streamID, timeout: timeout)
            }
            throw error
        }
    }

    /// Ends `streamID` on the daemon (`stream.cancel`): it purges the stream's queued items and ends it
    /// with `stream_end(canceled)`, which takes the route down. When the cancel fails, the route is
    /// dropped here: the stream's later lines are ignored until it ends on its own or the connection
    /// closes. A closed connection ended every stream already, so nothing is sent.
    func cancelResourceStream(_ streamID: String, timeout: Duration) async {
        guard !isClosed else { return }
        let params: [String: JSONValue] = ["stream": .string(streamID)]
        do {
            _ = try await sendStreamRequest(Self.streamCancel, params: params, timeout: timeout)
        } catch {
            dropStream(streamID)
        }
    }

    /// Sends one `cmux.protocol/2` request that takes no idempotency key (a stream open or cancel) and
    /// returns its `ok:true` response.
    private func sendStreamRequest(_ operation: DaemonCommandName, params: [String: JSONValue], timeout: Duration) async throws -> Response {
        try await request(cmd: operation, timeout: timeout) { id in
            try ResourceRequestEnvelope(id: id, operation: operation.wire, params: params, idempotencyKey: nil).line()
        }
    }

    /// The v2 operation that ends a stream on its connection.
    private static let streamCancel: DaemonCommandName = "stream.cancel"
}
