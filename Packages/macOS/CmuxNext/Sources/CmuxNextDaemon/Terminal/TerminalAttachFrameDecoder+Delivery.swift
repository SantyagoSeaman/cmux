import Foundation

extension TerminalAttachFrameDecoder {
    /// The stream's mapped output ring.
    struct RingReader: Sendable {
        let ring: any TerminalOutputRing
        /// The PTY offset of the first byte the ring held.
        let base: UInt64
        /// The ring's end when the RING was sent: where a stream with no screen offset yet starts.
        let sentEnd: UInt64
        /// The lap mark of the current screen.
        var laps: TerminalRingLaps
    }

    /// Contiguous OUTPUT frames back to back: the chunks of one output, or outputs with no sidecar between.
    /// An output's sidecar comes after its last chunk with the aux of its first (cmux-tui `attach_frames.rs`).
    struct OutputRun: Sendable {
        /// The aux of the run's first OUTPUT.
        var start: UInt64
        /// The offset after the run's last byte.
        var end: UInt64
        /// The offset after the last byte of the run that reached the view; nil while none has.
        var deliveredEnd: UInt64?

        /// Whether the sidecar with aux `offset` is of output in this run that reached the view.
        func carries(sidecarAt offset: UInt64) -> Bool {
            guard let deliveredEnd else { return false }
            return start <= offset && offset < deliveredEnd
        }
    }

    /// What the view's attachment does with the admissions the queue does not take itself.
    struct Delivery: Sendable {
        /// A `size-state` and this view's participant in it (the attachment orders them by generation).
        var sizeState: @Sendable (TerminalSizeState, String?) -> Void
        /// Sends `snapshot-request` for this stream, and calls the second argument (once, on any thread) when
        /// the request cannot be sent or the host refuses it (`LineTransport.sendNoReply`'s `onError`).
        var requestSnapshot: @Sendable (SnapshotRequestReason, _ failed: @escaping @Sendable () -> Void) -> Void
        /// The typing probe's decode mark for output, and for the key press it echoes when known.
        var outputDecoded: @Sendable (UInt64?) -> Void
        /// Closes the connection with `bad_message` and this reason: a RING or RING_ADVANCE was refused. The
        /// stream itself ends with `connectionLost` either way, so the view reattaches on the next connection.
        var closeConnection: @Sendable (String) -> Void = { _ in }
    }

    /// Reader thread. Hands one admission to the view's queue in stream order, as a decoded attach line is:
    /// output stays a slice of its read buffer, a key press's echo rides behind its chunk, `closed` ends the
    /// stream, and a gap asks the host for a fresh READY; when that request fails no READY is due, so the
    /// stream ends with `overflow` and the view reattaches.
    static func deliver(_ admission: TerminalFrameAdmission, to queue: TerminalEventQueue, delivery: Delivery) {
        switch admission {
        case .nothing:
            return
        case .output(let span):
            delivery.outputDecoded(nil)
            queue.push(span)
        case .ringOutput(let span, let wrapped):
            delivery.outputDecoded(nil)
            queue.push(span)
            guard let wrapped else { return }
            queue.push(wrapped)
        case .event(.sizeState(let state, let participant), _):
            delivery.sizeState(state, participant)
        case .event(.closed(let reason), _):
            queue.finish(.closed(reason))
        case .event(.inputEcho(let flow), _):
            delivery.outputDecoded(flow)
            queue.pushWithoutWaiting(.inputEcho(flow: flow))
        case .event(.output(let data, let colors), let flow):  // an `output` line
            delivery.outputDecoded(flow)
            queue.push(.output(data, colors: colors))
            guard let flow else { return }
            queue.pushWithoutWaiting(.inputEcho(flow: flow))
        case .event(let event, let flow):
            queue.push(event)
            guard let flow else { return }
            delivery.outputDecoded(flow)
            queue.pushWithoutWaiting(.inputEcho(flow: flow))
        case .resync(let reason):
            delivery.requestSnapshot(reason) { queue.finish(.closed(.overflow)) }
        case .refused(let refusal):
            delivery.closeConnection(refusal.reason)
            queue.finish(.closed(.connectionLost("bad_message: \(refusal.reason)")))
        }
    }
}
