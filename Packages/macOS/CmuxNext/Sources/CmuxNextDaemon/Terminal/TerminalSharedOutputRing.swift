import CmuxNextIPC
import Foundation

/// A ring stream's ``TerminalOutputRing`` over p05-ipc-112's ``SharedOutputRing``: the read-only mapping of the
/// daemon's ring, its spans, seqlock checks and consumer cursor, with its types mapped one to one.
final class TerminalSharedOutputRing: TerminalOutputRing {
    let shared: SharedOutputRing

    init(_ shared: SharedOutputRing) {
        self.shared = shared
    }

    /// Maps a RING frame's descriptor (taking it) and checks the mapping against the frame's grant.
    static let mapper: TerminalRingMapper = {
        (descriptor: Int32, grant: TerminalRingGrant) throws(TerminalRingRefusal) -> any TerminalOutputRing in
        let shared: SharedOutputRing
        do throws(SharedOutputRing.OpenError) {
            shared = try SharedOutputRing(adopting: descriptor)
        } catch {
            throw TerminalRingRefusal.mapping(TerminalSharedOutputRing.reason(error))
        }
        guard shared.capacity == grant.capacity else { throw TerminalRingRefusal.mapping("capacity") }
        return TerminalSharedOutputRing(shared)
    }

    var capacity: UInt64 { shared.capacity }
    var dataAddresses: Range<UInt> { shared.dataAddresses }

    func cut(from: UInt64, to: UInt64) throws(TerminalRingCutError) -> TerminalRingCut {
        do throws(SharedOutputRing.SpanError) {
            let spans = try shared.cut(from: from, to: to)
            return TerminalRingCut(range: spans.range, head: spans.head, tail: spans.tail)
        } catch {
            switch error {
            case .lapped(let from, let reserve): throw .lapped(from: from, reserve: reserve)
            case .notPublished(let to, let end): throw .notPublished(to: to, end: end)
            case .beforeBase(let from, let base): throw .beforeBase(from: from, base: base)
            case .backwards(let from, let to): throw .backwards(from: from, to: to)
            }
        }
    }

    func consumed(through offset: UInt64) {
        shared.consumed(through: offset)
    }

    func takeAck() -> UInt64? {
        shared.takeAck()
    }

    private static func reason(_ error: SharedOutputRing.OpenError) -> String {
        switch error {
        case .writable: "writable"
        case .statFailed: "stat"
        case .mapFailed: "map"
        case .tooSmall: "size"
        case .badMagic: "magic"
        case .badLayout: "layout"
        case .badCapacity: "capacity"
        case .endBeforeBase: "end_before_base"
        }
    }
}

/// The RING_ACKs of one ring stream (p05-ipc-119): once per display frame, ``displayFrame(ring:)`` sends the
/// stream's consumer cursor when it moved, so the app acks at most once per frame however much output arrives
/// (app-daemon-connection-DESIGN.md section 4.6). `send` writes the frame on the connection, at input priority:
/// the daemon holds a stalled viewer's READY until this ack reaches its gap.
struct TerminalRingAcks: Sendable {
    let stream: UInt16
    let send: @Sendable (_ frame: Data) -> Void

    /// The display frame ended: the RING_ACK of `ring`'s cursor, when it moved since the last one.
    @discardableResult
    func displayFrame(ring: any TerminalOutputRing) -> UInt64? {
        guard let offset = ring.takeAck() else { return nil }
        send(Data(SharedOutputRing.ringAckFrame(stream: stream, offset: offset)))
        return offset
    }
}
