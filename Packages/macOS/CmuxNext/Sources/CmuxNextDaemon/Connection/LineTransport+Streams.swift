import CmuxNextWakeups
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

    /// Gets the lines of every stream that is neither on the control sequence nor routed to its own sink.
    func setStreamHandler(_ handler: (@Sendable (_ streamID: String, _ line: Data) -> Void)?) { state.withLock { $0.streamHandler = handler } }

    /// Counts `streamID`'s lines as stream-0 events until its `stream_end`;
    /// call before the request that opens it, `endControlSequence` if that fails.
    func routeOnControlSequence(streamID: String) { _ = state.withLock { $0.controlStreams.insert(streamID) } }
    func endControlSequence(streamID: String) { _ = state.withLock { $0.controlStreams.remove(streamID) } }

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

    /// Opens a stream on this connection by the open rules of cmux-tui/spec/resource-api-v2.md: the open
    /// counts once its `ok:true` answer decodes and names `streamID`. An `ok:false` answer rejects it
    /// conclusively, and an open that never reached the socket has nothing to cancel: both only drop the
    /// route. Any other failure (no answer in time, or one that does not decode or names another stream)
    /// releases the route with ``cancelResourceStream(_:timeout:)``. The open's error is what the caller sees.
    func openResourceStream<R: Decodable & Sendable>(_ streamID: String, operation: DaemonCommandName,
                                                     params: [String: JSONValue], timeout: Duration, as type: R.Type,
                                                     sink: @escaping StreamSink) async throws -> ResourceStreamOpened<R> {
        routeStream(streamID, to: sink)
        var dispatched = false
        do {
            let response = try await sendStreamRequest(operation, params: params, timeout: timeout) { dispatched = true }
            let ack = try ResourceRequestEnvelope.decodeResult(StreamAck<R>.self, from: response)
            guard ack.streamID == streamID else {
                throw DaemonError.malformedResponse("\(operation.wire) acknowledged stream \(ack.streamID), not \(streamID)")
            }
            return ResourceStreamOpened(result: ack.result, transport: self, recorder: response.recorder)
        } catch {
            if let refused = error as? DaemonError, case .command = refused {
                dropStream(streamID)
            } else if dispatched {
                await cancelResourceStream(streamID, timeout: timeout)
            } else {
                dropStream(streamID)
            }
            throw error
        }
    }

    /// Ends `streamID` on the daemon (`stream.cancel`) by the cancel rules of
    /// cmux-tui/spec/resource-api-v2.md: it succeeds only when the exact empty response and the stream's
    /// `stream_end(canceled)` both arrive within `timeout`, in either order. Otherwise the connection is
    /// abandoned (``abandon(_:)``), which ends every request and stream on it, and its owner reconnects.
    /// A stream that ended already, or that another cancel is releasing, sends nothing.
    func cancelResourceStream(_ streamID: String, timeout: Duration) async {
        let (ends, endSeen) = AsyncStream<Bool>.makeStream(bufferingPolicy: .bufferingNewest(1))
        guard claimCancel(streamID, onEnd: { canceled in
            endSeen.yield(canceled)
            endSeen.finish()
        }) else { return }
        let deadline = DemandTimer(owner: "LineTransport.streamCancel", clock: clock)
        deadline.schedule(after: timeout) { endSeen.finish() }
        var confirmed = false
        let response = try? await sendStreamRequest(Self.streamCancel, params: ["stream": .string(streamID)], timeout: timeout)
        if let response, (try? ResourceRequestEnvelope.decodeResult([String: JSONValue].self, from: response))?.isEmpty == true {
            var iterator = ends.makeAsyncIterator()
            confirmed = await iterator.next() == true
        }
        deadline.cancel()
        _ = state.withLock { $0.cancelingStreams.remove(streamID) }
        if !confirmed { abandon("stream.cancel of \(streamID) was not confirmed") }
    }

    /// Closes the connection because a stream's cleanup was not confirmed, so its state can no longer be
    /// trusted: every request and stream on it fails with `detail`, and its owner reconnects as after a
    /// lost connection.
    func abandon(_ detail: String) {
        guard !isClosed else { return }
        failAll(.lost(detail))
        close()
    }

    /// Takes `streamID`'s one cancel claim and reports its `stream_end` to `onEnd` after its sink (true
    /// when the reason is `canceled`), or false at the close. False when the stream is not routed here
    /// (it ended, or the connection closed) or a cancel holds the claim already.
    private func claimCancel(_ streamID: String, onEnd: @escaping @Sendable (Bool) -> Void) -> Bool {
        let decoder = decoder
        return state.withLock { state -> Bool in
            guard let sink = state.streamSinks[streamID], state.cancelingStreams.insert(streamID).inserted else { return false }
            state.streamSinks[streamID] = { delivery in
                sink(delivery)
                switch delivery {
                case .item: break
                case .end(let line): onEnd((try? decoder.decode(StreamEnd.self, from: line))?.reason == "canceled")
                case .closed: onEnd(false)
                }
            }
            return true
        }
    }

    /// A stream open's `result`: its `stream_id` and the operation's own fields, read in one decode.
    private struct StreamAck<R: Decodable>: Decodable {
        var streamID: String
        var result: R

        private enum CodingKeys: String, CodingKey { case streamID = "stream_id" }

        init(from decoder: any Decoder) throws {
            streamID = try decoder.container(keyedBy: CodingKeys.self).decode(String.self, forKey: .streamID)
            result = try R(from: decoder)
        }
    }

    /// The field of a `stream_end` line that says how the stream ended.
    private struct StreamEnd: Decodable {
        var reason: String?
    }

    /// Sends one `cmux.protocol/2` request that takes no idempotency key (a stream open or cancel) and
    /// returns its `ok:true` response. `encoded` runs once the line is built, just before it is written.
    private func sendStreamRequest(_ operation: DaemonCommandName, params: [String: JSONValue], timeout: Duration,
                                   encoded: () -> Void = {}) async throws -> Response {
        try await request(cmd: operation, timeout: timeout) { id in
            let line = try ResourceRequestEnvelope(id: id, operation: operation.wire, params: params, idempotencyKey: nil).line()
            encoded()
            return line
        }
    }

    /// The v2 operation that ends a stream on its connection.
    private static let streamCancel: DaemonCommandName = "stream.cancel"
}
