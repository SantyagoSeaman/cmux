import CmuxNextCompat
import Darwin
import Foundation
@testable import CmuxNextDaemon

/// An output ring in this process: a shared anonymous mapping the test writes as the daemon's writer does
/// (`reserve`, copy, `end`; app-daemon-connection-DESIGN.md section 4.2), read through ``TerminalOutputRing``
/// with the rules of p05-ipc-112's `SharedOutputRing.cut`.
final class InProcessRing: TerminalOutputRing {
    let capacity: UInt64
    /// The data area's first byte.
    private let address: UInt
    private let reserve = Atomic<UInt64>(0)
    private let published = Atomic<UInt64>(0)
    private let spans = Atomic<Int>(0)
    private let mapped = Atomic<Int>(0)

    init(capacity: Int) {
        self.capacity = UInt64(capacity)
        // SAFETY: a fresh anonymous mapping of `capacity` bytes, unmapped once in deinit.
        let base = unsafe mmap(nil, capacity, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0)
        precondition(unsafe base != UnsafeMutableRawPointer(bitPattern: -1), "mmap failed: \(errno)")
        address = unsafe UInt(bitPattern: base)
    }

    deinit {
        // SAFETY: the mapping made in init; no span is left (each one retains the ring).
        unsafe munmap(UnsafeMutableRawPointer(bitPattern: address), Int(capacity))
    }

    /// Addresses of the data area.
    var mapping: Range<UInt> { address ..< address + UInt(capacity) }
    var dataAddresses: Range<UInt> { mapping }
    /// The offset after the last published byte.
    var end: UInt64 { published.load(ordering: .acquiring) }
    /// Spans handed out and not yet released.
    var liveSpans: Int { spans.load(ordering: .acquiring) }
    /// Descriptors the mapper took.
    var descriptorsMapped: Int { mapped.load(ordering: .acquiring) }

    /// The mapper of a ring stream that maps this ring for any matching grant (and closes the descriptor).
    var mapper: TerminalRingMapper {
        { (descriptor: Int32, grant: TerminalRingGrant) throws(TerminalRingRefusal) -> any TerminalOutputRing in
            Darwin.close(descriptor)
            guard grant.capacity == self.capacity else { throw TerminalRingRefusal.mapping("size") }
            self.mapped.add(1, ordering: .relaxed)
            return self
        }
    }

    /// A mapper that finds the descriptor maps writable, as p05-ipc-112's check refuses it.
    static let refusingMapper: TerminalRingMapper = {
        (descriptor: Int32, _: TerminalRingGrant) throws(TerminalRingRefusal) -> any TerminalOutputRing in
        Darwin.close(descriptor)
        throw TerminalRingRefusal.mapping("writable")
    }

    /// Publishes `bytes` at the end, as the daemon's writer: reserve, copy (one or two runs), end.
    func write(_ bytes: [UInt8]) {
        let start = end
        let count = UInt64(bytes.count)
        precondition(count <= capacity)
        reserve.store(start + count, ordering: .sequentiallyConsistent)
        let position = Int(start % capacity)
        let first = min(bytes.count, Int(capacity) - position)
        unsafe bytes.withUnsafeBytes { source in
            // SAFETY: `first` bytes from `position` and the rest from 0 lie inside the data area.
            unsafe UnsafeMutableRawPointer(bitPattern: address + UInt(position))?
                .copyMemory(from: source.baseAddress!, byteCount: first)
            if first < bytes.count {
                unsafe UnsafeMutableRawPointer(bitPattern: address)?
                    .copyMemory(from: source.baseAddress! + first, byteCount: bytes.count - first)
            }
        }
        published.store(start + count, ordering: .releasing)
    }

    /// A writer that has reserved `count` bytes past the end and not published them yet.
    func beginWrite(count: UInt64) {
        reserve.store(end + count, ordering: .sequentiallyConsistent)
    }

    /// `SharedOutputRing.cut`: backwards, unpublished, lapped (longer than the ring, or reserved past
    /// `from + capacity`), then one span or two across the wrap.
    func cut(from: UInt64, to: UInt64) throws(TerminalRingCutError) -> TerminalRingCut {
        guard to >= from else { throw .backwards(from: from, to: to) }
        let published = end
        guard to <= published else { throw .notPublished(to: to, end: published) }
        let reserved = reserve.load(ordering: .acquiring)
        guard to - from <= capacity, reserved <= from + capacity else { throw .lapped(from: from, reserve: reserved) }
        guard to > from else { return TerminalRingCut(range: from ..< to, head: nil, tail: nil) }
        let at = from % capacity
        let first = min(to - from, capacity - at)
        let head = span(at: at, count: first)
        let tail = to - from > first ? span(at: 0, count: to - from - first) : nil
        return TerminalRingCut(range: from ..< to, head: head, tail: tail)
    }

    private func span(at offset: UInt64, count: UInt64) -> Data {
        spans.add(1, ordering: .relaxed)
        // SAFETY: `count` bytes from `offset` lie in the data area (cut splits at the wrap), and the deallocator
        // holds the ring, so the mapping outlives the Data.
        let pointer = unsafe UnsafeMutableRawPointer(bitPattern: address + UInt(offset))!
        return unsafe Data(bytesNoCopy: pointer, count: Int(count), deallocator: .custom { _, _ in
            self.spans.add(-1, ordering: .relaxed)
        })
    }
}

/// A mapper that hands out `rings` in order, one per RING frame.
final class RingSequence: Sendable {
    private let rings: Mutex<[InProcessRing]>

    init(_ rings: [InProcessRing]) {
        self.rings = Mutex(rings)
    }

    var mapper: TerminalRingMapper {
        { (descriptor: Int32, _: TerminalRingGrant) throws(TerminalRingRefusal) -> any TerminalOutputRing in
            Darwin.close(descriptor)
            guard let ring = self.rings.withLock({ $0.isEmpty ? nil : $0.removeFirst() }) else {
                throw TerminalRingRefusal.mapping("none left")
            }
            return ring
        }
    }
}

/// What the decoder asked the attachment for.
final class RingHooks: Sendable {
    let requests = Mutex<[SnapshotRequestReason]>([])
    let closes = Mutex<[String]>([])

    var delivery: TerminalAttachFrameDecoder.Delivery {
        TerminalAttachFrameDecoder.Delivery(
            sizeState: { _, _ in },
            requestSnapshot: { reason, _ in self.requests.withLock { $0.append(reason) } },
            outputDecoded: { _ in },
            closeConnection: { reason in self.closes.withLock { $0.append(reason) } })
    }
}
