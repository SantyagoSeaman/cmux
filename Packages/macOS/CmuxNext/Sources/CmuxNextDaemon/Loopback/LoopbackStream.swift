public import Foundation
import CmuxNextCompat
import class CmuxSafeConcurrency.OnceContinuation
import func CmuxSafeConcurrency.withOnceThrowingContinuation

/// Writes one line on the stream's daemon connection (id-less, no reply).
protocol LoopbackLineSending: Sendable {
    /// False once the connection is gone.
    func send(_ line: Data) -> Bool
}

/// One `loopback-forward-v1` stream: bytes to and from a loopback port of
/// the daemon's machine, with credit flow control in both directions.
///
/// Writes wait (without blocking a thread) while the daemon window is
/// spent; received bytes stay within this client's window, so buffers are
/// bounded on both ends. All methods are safe from any thread.
///
/// Received bytes come as `loopback-data` lines or, on a forward that rides a frame stream, as OUTPUT frames
/// whose payload is the bytes and whose aux is their offset in what the target sent (p05-ipc-149). Both feed
/// one credit queue: it holds at most `receiveWindow` bytes, so at most that many data events (each holds a
/// byte or more) plus the end of the target's side and the close.
public final class LoopbackStream: Sendable {
    public let id: UInt64
    public let events: AsyncStream<LoopbackStreamEvent>

    /// Largest payload of one `loopback-data` line.
    static let frameBytes = 64 * 1024

    private let continuation: AsyncStream<LoopbackStreamEvent>.Continuation
    private let sender: any LoopbackLineSending
    private let receiveWindow: Int
    private let onFinish: @Sendable (UInt64) -> Void

    private struct State {
        var address = ""
        var sendCredit = 0
        /// Received and not yet consumed.
        var buffered = 0
        /// Consumed and not yet granted back.
        var toGrant = 0
        /// Bytes the target sent on this forward and this client received: where the next OUTPUT frame starts.
        var receivedOffset: UInt64 = 0
        var writeWaiter: OnceContinuation<Void, any Error>?
        var writeShutdown = false
        var finished: LoopbackStreamError??
    }

    private let state: Mutex<State>

    /// The credit queue's event bound for `receiveWindow`: one data event per byte at most, then `.eof` and
    /// `.closed`.
    static func queueCapacity(receiveWindow: Int) -> Int { max(receiveWindow, 0) + 2 }

    /// Registered before `loopback-open` is sent, so a target that speaks
    /// first loses nothing; `opened` adds the daemon window.
    init(id: UInt64, receiveWindow: Int, sender: any LoopbackLineSending,
         onFinish: @escaping @Sendable (UInt64) -> Void) {
        self.id = id
        self.sender = sender
        self.receiveWindow = receiveWindow
        self.onFinish = onFinish
        state = Mutex(State())
        // memory: bounded-by receiveWindow (the credit window, 256 KiB): `receive` ends the stream past it
        (events, continuation) = AsyncStream.makeStream(
            of: LoopbackStreamEvent.self,
            bufferingPolicy: .bufferingOldest(Self.queueCapacity(receiveWindow: receiveWindow)))
    }

    /// The loopback address the daemon connected (`127.0.0.1:5173`).
    public var address: String { state.withLock(\.address) }

    func opened(address: String, sendWindow: Int) {
        let waiter = state.withLock { state -> OnceContinuation<Void, any Error>? in
            state.address = address
            state.sendCredit += sendWindow
            defer { state.writeWaiter = nil }
            return state.writeWaiter
        }
        waiter?.resume(returning: ())
    }

    /// `loopback-open` failed or timed out. The close line ends a stream the
    /// daemon may still open after a timeout (it ignores unknown streams).
    func abandon() {
        finish(.closedLocally, notifyDaemon: true)
    }

    // MARK: Client side

    /// Sends `data` to the target in frames, waiting for daemon credit.
    /// Throws once the stream has ended or after `shutdownWrite()`.
    public func write(_ data: Data) async throws {
        var rest = data
        while !rest.isEmpty {
            let allowed = try await reserveCredit(upTo: min(Self.frameBytes, rest.count))
            let frame = rest.prefix(allowed)
            guard sender.send(LoopbackForwardLine.data(stream: id, bytes: Data(frame))) else {
                finish(.connectionLost("write failed"), notifyDaemon: false)
                throw LoopbackStreamError.connectionLost("write failed")
            }
            rest = rest.dropFirst(allowed)
        }
    }

    /// `count` received bytes were written on; returns credit to the daemon
    /// in batches of a quarter window (or when the buffer is empty).
    public func consumed(_ count: Int) {
        guard count > 0 else { return }
        let grant: Int = state.withLock { state in
            guard state.finished == nil else { return 0 }
            state.buffered = max(0, state.buffered - count)
            state.toGrant += count
            guard state.toGrant >= receiveWindow / 4 || state.buffered == 0 else { return 0 }
            defer { state.toGrant = 0 }
            return state.toGrant
        }
        if grant > 0 { _ = sender.send(LoopbackForwardLine.credit(stream: id, bytes: grant)) }
    }

    /// No more bytes from this side; the target sees end of stream after the
    /// queued bytes. Received bytes keep arriving.
    public func shutdownWrite() {
        let send = state.withLock { state -> Bool in
            guard state.finished == nil, !state.writeShutdown else { return false }
            state.writeShutdown = true
            return true
        }
        if send { _ = sender.send(LoopbackForwardLine.shutdown(stream: id)) }
    }

    /// Aborts the stream.
    public func close() {
        finish(.closedLocally, notifyDaemon: true)
    }

    // MARK: Daemon side (reader thread)

    func deliver(_ event: LoopbackForwardEvent) {
        switch event {
        case .data(_, let bytes):
            receive(bytes, at: nil)
        case .credit(_, let bytes):
            let waiter = state.withLock { state -> OnceContinuation<Void, any Error>? in
                state.sendCredit += bytes
                defer { state.writeWaiter = nil }
                return state.writeWaiter
            }
            waiter?.resume(returning: ())
        case .eof:
            enqueue(.eof)
        case .closed(_, let error):
            finish(error.map { LoopbackStreamError.daemon($0) }, notifyDaemon: false)
        }
    }

    /// The payload of an OUTPUT frame on this forward's frame stream (p05-ipc-149): the target's bytes as they
    /// came, never base64, starting at `offset` in what the target sent. An offset that is not where the last
    /// bytes ended means bytes were lost or sent twice: the stream ends.
    func deliverOutput(_ bytes: Data, at offset: UInt64) {
        receive(bytes, at: offset)
    }

    /// Queues received bytes within the window; `offset`, when the frame names one, must continue the bytes
    /// before it.
    private func receive(_ bytes: Data, at offset: UInt64?) {
        enum Outcome { case queue, ignore, overflow, gap(expected: UInt64) }
        let outcome = state.withLock { state -> Outcome in
            guard state.finished == nil, !bytes.isEmpty else { return .ignore }
            if let offset, offset != state.receivedOffset { return .gap(expected: state.receivedOffset) }
            guard state.buffered + bytes.count <= receiveWindow else { return .overflow }
            state.buffered += bytes.count
            state.receivedOffset += UInt64(truncatingIfNeeded: bytes.count)
            return .queue
        }
        switch outcome {
        case .queue:
            enqueue(.data(bytes))
        case .ignore:
            break
        case .overflow:
            finish(.protocolViolation("the daemon sent past the window"), notifyDaemon: true)
        case .gap(let expected):
            finish(.protocolViolation("output at offset \(offset ?? 0), expected \(expected)"), notifyDaemon: true)
        }
    }

    /// One event into the credit queue. The window keeps it under its bound; an event past it means the daemon
    /// broke the protocol (a second end of stream, say), and the stream ends rather than drop it.
    private func enqueue(_ event: LoopbackStreamEvent) {
        if case .dropped = continuation.yield(event) {
            finish(.protocolViolation("the daemon sent past the queue bound"), notifyDaemon: true)
        }
    }

    /// The connection ended: every stream on it fails.
    func connectionLost(_ detail: String) {
        finish(.connectionLost(detail), notifyDaemon: false)
    }

    // MARK: Private

    private func reserveCredit(upTo wanted: Int) async throws -> Int {
        // wakeup-allow: bounded: each iteration either takes credit or waits for the next credit event or the end of the stream
        while true {
            let outcome: Result<Int, LoopbackStreamError>? = state.withLock { state in
                if let finished = state.finished { return .failure(finished ?? .closedLocally) }
                if state.writeShutdown { return .failure(.closedLocally) }
                guard state.sendCredit > 0 else { return nil }
                let allowed = min(wanted, state.sendCredit)
                state.sendCredit -= allowed
                return .success(allowed)
            }
            switch outcome {
            case .success(let allowed): return allowed
            case .failure(let error): throw error
            case nil: break
            }
            try await withTaskCancellationHandler {
                try await withOnceThrowingContinuation { (waiter: OnceContinuation<Void, any Error>) in
                    let resumeNow = state.withLock { state -> Bool in
                        if state.finished != nil || state.sendCredit > 0 || state.writeWaiter != nil { return true }
                        state.writeWaiter = waiter
                        return false
                    }
                    if resumeNow { waiter.resume(returning: ()) }
                }
            } onCancel: {
                self.close()
            }
        }
    }

    private func finish(_ error: LoopbackStreamError?, notifyDaemon: Bool) {
        let waiter: OnceContinuation<Void, any Error>?? = state.withLock { state in
            guard state.finished == nil else { return .none }
            state.finished = .some(error)
            defer { state.writeWaiter = nil }
            return .some(state.writeWaiter)
        }
        guard case .some(let pending) = waiter else { return }
        pending?.resume(throwing: error ?? .closedLocally)
        if notifyDaemon { _ = sender.send(LoopbackForwardLine.close(stream: id)) }
        continuation.yield(.closed(error: error))
        continuation.finish()
        onFinish(id)
    }
}
