import Foundation
import CmuxNextCompat
import CmuxNextIPC

/// The `loopback-forward-v1` streams of one daemon connection (p05-ipc-101). The connection owns it
/// (``LineTransport/loopbackStreams()``): stream ids are minted here, per connection, so every client that
/// forwards on the connection gets ids no other one holds, as Mojo's `MultiplexRouter` mints the interface ids of
/// one pipe (mojo/public/cpp/bindings/lib/multiplex_router.cc, `AssociateInterface`).
///
/// The reader thread only hands an event line over (``route(name:line:)``): its one decode and its
/// delivery run in order on the table's serial queue at the bulk class, so forwarded bytes never hold up the
/// replies and events behind them on the connection (p05-ipc design 3.4: the reader cuts and routes, bulk decode runs
/// off it). The queue holds at most each stream's receive window of data: the daemon sends no more before credit,
/// and a stream it overruns ends (``LoopbackStream``).
final class LoopbackStreamTable: Sendable {
    private struct Entry {
        /// The client that opened the stream (``LoopbackForwardClient``), so its `close()` ends only its own.
        let owner: ObjectIdentifier
        let stream: LoopbackStream
    }

    private struct Streams {
        var byID: [UInt64: Entry] = [:]
        var nextID: UInt64 = 1
        /// Set once the connection ended: no stream is added after it.
        var ended = false
    }

    /// The name prefix of every event of the protocol (`loopback-data`, `loopback-credit`, `loopback-eof`,
    /// `loopback-closed`).
    static let eventPrefix = "loopback-"

    private let streams = Mutex(Streams())
    /// Decodes and delivers the connection's loopback events, in wire order.
    private let deliveries: DispatchQueue
    /// Used only on ``deliveries``; it counts into the connection's recorder.
    private let decoder: WireDecoder

    init(decoder: WireDecoder) {
        self.decoder = decoder
        deliveries = DispatchQueue(label: "com.cmuxterm.next.daemon.loopback", qos: DaemonWireStats.Role.loopbackForward.writerQoS)
    }

    /// A new stream of `owner` with this connection's next id, registered before its `loopback-open` is sent so
    /// a target that speaks first loses nothing. Nil once the connection ended.
    func makeStream(owner: ObjectIdentifier, receiveWindow: Int, sender: any LoopbackLineSending) -> LoopbackStream? {
        streams.withLock { streams -> LoopbackStream? in
            guard !streams.ended else { return nil }
            let id = streams.nextID
            streams.nextID += 1
            let stream = LoopbackStream(id: id, receiveWindow: receiveWindow, sender: sender, onFinish: { self.remove($0) })
            streams.byID[id] = Entry(owner: owner, stream: stream)
            return stream
        }
    }

    func remove(_ id: UInt64) {
        _ = streams.withLock { $0.byID.removeValue(forKey: id) }
    }

    func stream(_ id: UInt64) -> LoopbackStream? {
        streams.withLock { $0.byID[id]?.stream }
    }

    /// Takes `owner`'s streams out of the table.
    func removeAll(owner: ObjectIdentifier) -> [LoopbackStream] {
        streams.withLock { streams in
            let owned = streams.byID.filter { $0.value.owner == owner }
            for id in owned.keys { streams.byID[id] = nil }
            return owned.values.map(\.stream)
        }
    }

    /// `owner`'s open streams.
    func count(owner: ObjectIdentifier) -> Int {
        streams.withLock { $0.byID.values.count(where: { $0.owner == owner }) }
    }

    /// Reader thread. Queues one `loopback-*` event line; its stream gets it after every earlier one.
    func route(name: String, line: Data) {
        deliveries.async { [self] in
            guard let event = LoopbackForwardEvent.decode(name: name, line: line, decoder: decoder) else { return }
            stream(event.stream)?.deliver(event)
        }
    }

    /// The connection ended: no stream is added from now on, and each open one fails with `detail` after the
    /// events routed before the end.
    func end(_ detail: String) {
        streams.withLock { $0.ended = true }
        deliveries.async { [self] in
            let ended = streams.withLock { streams in
                defer { streams.byID.removeAll() }
                return streams.byID.values.map(\.stream)
            }
            for stream in ended { stream.connectionLost(detail) }
        }
    }
}

/// Writes a stream's lines on its connection at bulk priority: behind the person's input and the control
/// sequence, in one FIFO with every other bulk line, so a stream's data, half close and close keep their order
/// (p05-ipc design 3.4, outbound order).
struct TransportLineSender: LoopbackLineSending {
    let transport: LineTransport

    func send(_ line: Data) -> Bool { transport.sendLine(line, priority: .bulk) }
}

extension LineTransport {
    /// This connection's loopback streams, made on first use; nil once the connection closed, so no stream joins a
    /// table whose end already ran (``failAll`` ends it under the same lock that sets `closed`).
    func loopbackStreams() -> LoopbackStreamTable? {
        state.withLock { state -> LoopbackStreamTable? in
            guard state.closed == nil else { return nil }
            if let table = state.loopbackStreams { return table }
            let table = LoopbackStreamTable(decoder: WireCoding.decoder(recorder))
            state.loopbackStreams = table
            return table
        }
    }

    /// Reader thread. Hands a `loopback-*` event to the connection's table, never to the control sequence (it does
    /// not move ``routedEventCount``); false for any other event.
    func routeLoopback(event: String, line: Data) -> Bool {
        guard event.hasPrefix(LoopbackStreamTable.eventPrefix) else { return false }
        guard let table = state.withLock({ $0.loopbackStreams }) else {
            recorder?.lineRead(.ignored)
            return true
        }
        recorder?.lineRead(.streamItem)
        table.route(name: event, line: line)
        return true
    }
}

extension TransportCloseReason {
    /// The words a stream that ended with its connection reports.
    var detail: String {
        switch self {
        case .closedByClient: "closed"
        case .daemonShutdown: "daemon shut down"
        case .lost(let text): text
        }
    }
}
