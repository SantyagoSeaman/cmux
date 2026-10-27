import CmuxNextWakeups
public import Foundation

/// A bounded read of a session journal (`session.journal.subscribe` with `follow:false`, the
/// primitive behind `cmux session current journal read`; cmux-tui/spec/resource-api-v2.md) as a
/// stream on the machine's one control connection (plans/cmux-next/chrome-parity/p05-ipc.md 3.1):
/// no connection or handshake of its own. The read is bulk and independent: its lines go to its own
/// sink, never to the control sequence or a reply's event barrier, and the connection's reader only
/// hands them on; each is parsed once, here, off the reader thread and off the caller's actor. Chrome
/// keeps such reads off the IO thread the same way: a Mojo data pipe's consumer reads on its own
/// sequence (mojo/public/cpp/system/data_pipe_drainer.h).
public struct SessionJournalRead: Sendable {
    /// The v2 operation the read sends.
    private static let subscribe: DaemonCommandName = "session.journal.subscribe"
    /// The `stream.cancel` deadline of a read that stops before its stream ends.
    private static let cancelTimeout: Duration = .seconds(2)

    public struct Result: Sendable {
        /// Each record's JSON (the stream item), in journal order.
        public var records: [Data]
        /// The journal's session id (the cursor generation).
        public var generation: String?
        /// The last delivered sequence; resume after it next time.
        public var lastSequence: UInt64?
        /// True when the stream ended with `completed` (not cut short).
        public var complete: Bool
    }

    private let carrier: any ResourceStreamCarrier

    /// Reads on `connection`, the machine's control connection.
    public init(connection: DaemonConnection) {
        carrier = connection
    }

    /// Reads on `carrier` (tests: a transport on a fake daemon).
    init(carrier: any ResourceStreamCarrier) {
        self.carrier = carrier
    }

    /// Reads records of `kinds` after `cursor` (nil: from the beginning), at most `limit` of them, giving
    /// up `deadline` after the open (which has the same deadline). A read that stops before the stream
    /// ends cancels it on the daemon. The traffic counts into the connection's ``DaemonWireStats``.
    @concurrent
    public func read(kinds: [String], cursor: (generation: String, sequence: UInt64)?,
                     limit: Int = 20_000, deadline: Duration = .seconds(5)) async throws -> Result {
        let streamID = "stream_" + UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
        // Never more than `limit` items and the end wait here: a long journal is not held past the limit.
        let (deliveries, continuation) = AsyncStream<LineTransport.StreamDelivery>.makeStream(
            bufferingPolicy: .bufferingOldest(max(limit, 0) + 1))
        var params: [String: JSONValue] = [
            "machine": .string("current"), "session": .string("current"), "stream_id": .string(streamID), "follow": .bool(false),
            "filter": .object(["kinds": .array(kinds.map(JSONValue.string)), "max_sensitivity": .string("sensitive")]),
        ]
        if let cursor {
            params["cursor"] = .object(["generation": .string(cursor.generation), "revision": .string(String(cursor.sequence))])
        } else {
            params["start"] = .string("beginning")
        }
        let opened = try await carrier.openResourceStream(streamID, operation: Self.subscribe, params: params, timeout: deadline,
                                                          as: Opened.self) { continuation.yield($0) }
        let timer = DemandTimer(owner: "SessionJournalRead.deadline")
        timer.schedule(after: deadline) { continuation.finish() }
        var collector = Collector(limit: limit, generation: opened.result.cursor?.generation, recorder: opened.recorder)
        for await delivery in deliveries {
            if collector.take(delivery) { break }
        }
        timer.cancel()
        continuation.finish()
        if !collector.streamOver { await opened.transport.cancelResourceStream(streamID, timeout: Self.cancelTimeout) }
        return collector.result
    }

    /// The open's `result` (`StreamOpened`).
    private struct Opened: Decodable, Sendable {
        struct Cursor: Decodable, Sendable { var generation: String? }
        var cursor: Cursor?
    }

    /// The stream's lines, folded in wire order by the reading task.
    private struct Collector {
        private(set) var result: Result
        /// The stream ended (`stream_end`) or its connection closed: there is nothing to cancel.
        private(set) var streamOver = false
        private let limit: Int
        private let recorder: DaemonWireStats.Recorder?

        init(limit: Int, generation: String?, recorder: DaemonWireStats.Recorder?) {
            result = Result(records: [], generation: generation, lastSequence: nil, complete: false)
            self.limit = limit
            self.recorder = recorder
        }

        /// Takes one delivery; true once the read is over.
        mutating func take(_ delivery: LineTransport.StreamDelivery) -> Bool {
            switch delivery {
            case .item(let line):
                guard let object = parse(line) else { return false }
                if let item = object["item"], JSONSerialization.isValidJSONObject(item),
                   let record = try? JSONSerialization.data(withJSONObject: item) {
                    result.records.append(record)
                    if let revision = cursor(of: object)?["revision"] as? String, let sequence = UInt64(revision) {
                        result.lastSequence = sequence
                    }
                }
                return result.records.count >= limit
            case .end(let line):
                streamOver = true
                result.complete = parse(line)?["reason"] as? String == "completed"
                return true
            case .closed:
                streamOver = true
                return true
            }
        }

        /// The line's one parse; it carries the cursor's generation along.
        private mutating func parse(_ line: Data) -> [String: Any]? {
            recorder?.decoded()
            guard let object = try? JSONSerialization.jsonObject(with: line) as? [String: Any] else { return nil }
            if let generation = cursor(of: object)?["generation"] as? String { result.generation = generation }
            return object
        }

        private func cursor(of object: [String: Any]) -> [String: Any]? {
            object["cursor"] as? [String: Any]
        }
    }
}

extension DaemonConnection: ResourceStreamCarrier {
    /// Opens the stream on the transport that is ready now, after a quiet handoff, as every request
    /// does, and counted in flight like one until the open is answered.
    func openResourceStream<R: Decodable & Sendable>(_ streamID: String, operation: DaemonCommandName,
                                                     params: [String: JSONValue], timeout: Duration, as type: R.Type,
                                                     sink: @escaping LineTransport.StreamSink) async throws -> ResourceStreamOpened<R> {
        await waitOutHandoff()
        guard let ready else { throw DaemonError.notConnected }
        requestStarted()
        defer { requestEnded() }
        return try await ready.transport.openResourceStream(streamID, operation: operation, params: params, timeout: timeout,
                                                            as: type, sink: sink)
    }
}
