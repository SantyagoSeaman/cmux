import CmuxNextIPC
import Foundation

extension LineTransport {
    /// The reader's next whole message (nil: it needs more bytes), or the bad message that ends the connection.
    static func nextMessage(_ reader: inout FrameReader) -> Result<FrameReader.Message?, BadMessage> {
        Result { () throws(BadMessage) -> FrameReader.Message? in try reader.next() }
    }

    /// Routes one line by its fields, on the reader thread. ``LineRouteScan`` reads them from the reader's
    /// bytes, so a reply or an event is parsed once, by its reader. Only a line the scan cannot read is decoded
    /// as an ``Envelope`` here, and a reply that is not `ok:true`, once, for its error fields.
    ///
    /// Returns false once the connection is closed (``reportBadMessage(reason:detail:)``, `close()`): that line
    /// is not routed and the reader stops, so nothing read after a bad message reaches a handler or a waiter
    /// (Chrome's ReportBadMessage). The check sits in the state lock each routed line takes anyway.
    @discardableResult
    func route<Line: RandomAccessCollection<UInt8>>(_ bytes: Line, decoder: WireDecoder, onEvent: CausedEventHandler) -> Bool
    where Line.Index == Int {
        let line = Data(bytes)
        let envelope: Envelope
        var decoded = false
        if let scanned = LineRouteScan(bytes) {
            envelope = scanned.envelope
        } else if let full = try? decoder.decode(Envelope.self, from: line) {
            envelope = full
            decoded = true
        } else {
            recorder?.lineRead(.ignored)
            return true
        }
        // A v2 stream line goes to the stream handler of a dedicated
        // connection (`SessionJournalRead`); on the control connection it
        // travels with the raw events, in socket order (`session.events`).
        let streamed = envelope.type == "stream_item" || envelope.type == "stream_end"
        let streamKind: DaemonWireStats.FrameKind = envelope.type == "stream_end" ? .streamEnd : .streamItem
        if streamed, let streamID = envelope.streamID {
            let streamRoute = state.withLock { state -> StreamRoute? in
                guard state.closed == nil else { return nil }
                if state.controlStreams.contains(streamID) {
                    if envelope.type == "stream_end" { state.controlStreams.remove(streamID) }
                    return .control
                }
                if let handler = state.streamHandler { return .handler(handler) }
                return .unrouted
            }
            switch streamRoute {
            case nil: return false
            case .control: break
            case .handler(let handler):
                recorder?.lineRead(streamKind)
                handler(streamID, line)
                return true
            case .unrouted:
                recorder?.lineRead(.ignored)
                return true
            }
        }
        if let name = streamed ? Self.streamEvent : envelope.event {
            let index = state.withLock { state -> UInt64? in
                guard state.closed == nil else { return nil }
                if name == "daemon-shutdown" { state.sawShutdown = true }
                state.eventCount += 1
                return state.eventCount
            }
            guard let index else { return false }
            recorder?.lineRead(streamed ? streamKind : .event)
            onEvent(name, line, index, envelope.cause)
            return true
        }
        guard envelope.ok != nil || envelope.id != nil else {
            recorder?.lineRead(.ignored)
            return true
        }
        let routed = state.withLock { state -> ReplyRoute in
            // `failAll` failed every waiter when it closed the connection, under this lock.
            guard state.closed == nil else { return .closed }
            guard let id = envelope.id ?? state.pending.oldestID(), let entry = state.pending.remove(id: id) else { return .dropped }
            removeDeadline(&state, id: id)
            return .waiter(barrier: state.eventCount, entry)
        }
        let barrier: UInt64, entry: PendingRequest
        switch routed {
        case .closed: return false
        case .dropped:
            recorder?.lineRead(.droppedReply)
            return true
        case .waiter(let eventBarrier, let pending): (barrier, entry) = (eventBarrier, pending)
        }
        let waiter = entry.waiter
        if let sent = entry.sent { recorder?.replied(sent) }
        recorder?.lineRead(waiter.replyKind(ok: envelope.ok == true))
        if envelope.ok == true {
            if case .reply(_, let slot) = waiter {
                slot.resolve(.success(Response(line: line, eventBarrier: barrier, recorder: recorder)))
            }
            return true
        }
        guard let fail = waiter.failure else { return true }
        // The error's fields: this line's one parse (its waiter reads no data from it).
        let failure = decoded ? envelope : (try? decoder.decode(Envelope.self, from: line)) ?? envelope
        fail(DaemonError.command(cmd: waiter.cmd, message: failure.error ?? "unknown error", code: failure.errorCode,
                                 details: failure.errorDetails, retryable: failure.retryable))
        return true
    }

    /// Where one reply line goes (`route`).
    private enum ReplyRoute {
        /// The connection closed: the reader stops.
        case closed
        /// No waiter has its id (a reply after its deadline, or to a request that wanted none).
        case dropped
        /// Its waiter, with the stream-0 events routed before it.
        case waiter(barrier: UInt64, PendingRequest)
    }
}
