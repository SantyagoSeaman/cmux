import CmuxNextCompat
import Darwin
import Foundation
import Testing
@testable import CmuxNextDaemon

/// Ring streams (terminal-output-ring-v1, p05-ipc-113): a RING maps the terminal's output ring, a RING_ADVANCE
/// gives at most two spans that reach the view as `Data` pointing into the mapping (no copy), spans merge only
/// with the adjacent span of the same mapping, and a gap or a lap, at cut, in the queue or during the parse,
/// asks for a fresh READY.
struct TerminalRingTests {
    private static let surface = SurfaceID(rawValue: 3)
    private let wire = WireDecoder(recorder: nil)

    // MARK: Zero copies

    /// The acceptance of p05-ipc-113: 50 MB through a 1 MiB ring reach a view that keeps up with
    /// `outputBytesCopiedApp == 0`, every byte where the writer put it and every span's base address inside the
    /// mapping (the `Data` the lane hands to `ghostty_surface_process_output` unchanged).
    @Test func fiftyMegabytesReachTheViewFromTheMappingWithNoCopy() async throws {
        let ring = InProcessRing(capacity: 1 << 20)
        let stats = DaemonWireStats()
        let queue = TerminalEventQueue(recorder: stats.recorder())
        let hooks = RingHooks()
        var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: ring.mapper)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        #expect(await queue.next() == .snapshot(Self.readyFrame(offset: 0, generation: 1)))

        let chunk = 48 * 1024  // not a divisor of the capacity, so advances wrap mid-chunk
        var total = 0
        var index = 0
        var wraps = 0
        while total < 50_000_000 {
            let value = UInt8(truncatingIfNeeded: index % 251)
            ring.write([UInt8](repeating: value, count: chunk))
            let admission = decoder.admit(Self.advance(to: ring.end), decoder: wire)
            guard case .ringOutput(_, let wrapped) = admission else {
                Issue.record("advance \(index) gave \(admission)")
                return
            }
            if wrapped != nil { wraps += 1 }
            TerminalAttachFrameDecoder.deliver(admission, to: queue, delivery: hooks.delivery)
            var received = 0
            for _ in 0 ..< (wrapped == nil ? 1 : 2) {
                guard case .output(let data, nil)? = await queue.next() else {
                    Issue.record("advance \(index): no output")
                    return
                }
                #expect(ring.mapping.contains(Self.address(data)))
                #expect(data == Data(repeating: value, count: data.count))
                received += data.count
            }
            #expect(received == chunk)
            total += chunk
            index += 1
        }

        #expect(wraps > 0)
        #expect(stats.snapshot().outputBytesCopiedApp == 0)
        #expect(hooks.requests.withLock { $0 }.isEmpty)
        #expect(ring.liveSpans == 0)  // every span the view dropped released its hold on the mapping
    }

    /// A wrapping advance is two spans that never merge across the wrap; a backlog of spans that continue each
    /// other in the mapping merges into one wider range of it, and nothing merges across another event.
    @Test func spansMergeOnlyWithTheAdjacentSpanOfOneMapping() async throws {
        let ring = InProcessRing(capacity: 1024)
        let stats = DaemonWireStats()
        let queue = TerminalEventQueue(recorder: stats.recorder())
        let hooks = RingHooks()
        var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: ring.mapper)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        ring.write(Self.bytes(0x61, count: 700))
        deliver(Self.advance(to: 700), &decoder, queue, hooks)
        #expect(await queue.next() == .snapshot(Self.readyFrame(offset: 0, generation: 1)))
        #expect(await queue.next() == .output(Data(Self.bytes(0x61, count: 700)), colors: nil))
        // The view falls behind: everything below stays queued, and the writer never laps it.
        ring.write(Self.bytes(0x62, count: 300))  // 700 ..< 1000
        deliver(Self.advance(to: 1000), &decoder, queue, hooks)
        ring.write(Self.bytes(0x63, count: 300))  // 1000 ..< 1300 wraps at 1024
        let wrapping = decoder.admit(Self.advance(to: 1300), decoder: wire)
        guard case .ringOutput(let head, let wrapped?) = wrapping else {
            Issue.record("a wrapping advance gave \(wrapping)")
            return
        }
        #expect([head.offset, UInt64(head.count), wrapped.offset, UInt64(wrapped.count)] == [1000, 24, 1024, 276])
        TerminalAttachFrameDecoder.deliver(wrapping, to: queue, delivery: hooks.delivery)
        ring.write(Self.bytes(0x64, count: 100))  // 1300 ..< 1400 continues the wrapped span in memory
        deliver(Self.advance(to: 1400), &decoder, queue, hooks)
        queue.push(.resized(TerminalReplay(cols: 100, rows: 30, data: Data())))
        ring.write(Self.bytes(0x65, count: 100))  // continues in memory, but a grid replacement is between
        deliver(Self.advance(to: 1500), &decoder, queue, hooks)
        queue.finish()

        var events: [TerminalChannelEvent] = []
        var addresses: [UInt] = []
        while let event = await queue.next() {
            if case .output(let data, _) = event { addresses.append(Self.address(data)) }
            events.append(event)
        }
        #expect(events == [
            .output(Data(Self.bytes(0x62, count: 300) + Self.bytes(0x63, count: 24)), colors: nil),
            .output(Data(Self.bytes(0x63, count: 276) + Self.bytes(0x64, count: 100)), colors: nil),
            .resized(TerminalReplay(cols: 100, rows: 30, data: Data())),
            .output(Data(Self.bytes(0x65, count: 100)), colors: nil),
        ])
        // Merged by range: each event starts where its first span lies in the mapping, nothing was copied.
        let base = ring.mapping.lowerBound
        #expect(addresses == [base + 700, base, base + 376])
        #expect(stats.snapshot().outputBytesCopiedApp == 0)
    }

    // MARK: Gaps and laps

    /// An advance more than the ring past the cursor lost bytes: one `snapshot-request`, ring bytes dropped until
    /// the READY, and the stream continues from the READY's offset.
    @Test func anAdvanceMoreThanTheRingBehindIsAGapAndTheNextReadyResumes() async throws {
        let ring = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: ring.mapper)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        ring.write(Self.bytes(0x61, count: 800))
        ring.write(Self.bytes(0x62, count: 700))  // the view never saw 0 ..< 1500: more than the ring
        deliver(Self.advance(to: 1500), &decoder, queue, hooks)
        ring.write(Self.bytes(0x63, count: 50))
        deliver(Self.advance(to: 1550), &decoder, queue, hooks)  // dropped: the fresh READY is due
        deliver(Self.ready(offset: 1550, generation: 2), &decoder, queue, hooks)
        ring.write(Self.bytes(0x64, count: 64))
        deliver(Self.advance(to: 1614), &decoder, queue, hooks)
        queue.finish()

        #expect(hooks.requests.withLock { $0 } == [.gap])
        #expect(await Self.drain(queue) == [
            .snapshot(Self.readyFrame(offset: 0, generation: 1)),
            .snapshot(Self.readyFrame(offset: 1550, generation: 2)),
            .output(Data(Self.bytes(0x64, count: 64)), colors: nil),
        ])
    }

    /// The check at cut: the writer has started a chunk that overwrites the first byte the view still needs, so
    /// the bytes are not queued at all (they would reach the parser torn).
    @Test func aWriterAlreadyOverwritingTheRangeIsAGapAtCut() async throws {
        let ring = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: ring.mapper)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        ring.write(Self.bytes(0x61, count: 1000))
        ring.beginWrite(count: 100)  // reserve 1100: bytes 0 ..< 76 may be being overwritten
        let admission = decoder.admit(Self.advance(to: 1000), decoder: wire)

        guard case .resync(.gap) = admission else {
            Issue.record("a range the writer is overwriting gave \(admission)")
            return
        }
    }

    /// The check at release: the writer lapped a span while the view parsed it, so the screen may be torn; the
    /// next frame of the stream asks for a fresh READY, and a late release of the replaced screen's span does
    /// not mark the new screen.
    @Test func aLapDuringTheParseResyncsAtTheNextFrame() async throws {
        let ring = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: ring.mapper)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        _ = await queue.next()  // the READY
        ring.write(Self.bytes(0x61, count: 600))
        deliver(Self.advance(to: 600), &decoder, queue, hooks)
        do {
            guard case .output(let parsing, nil)? = await queue.next() else {
                Issue.record("no output")
                return
            }
            withExtendedLifetime(parsing) {
                // The view parses 0 ..< 600 while the writer laps it.
                ring.write(Self.bytes(0x62, count: 600))
                ring.write(Self.bytes(0x63, count: 500))
            }
        }
        // `parsing` is gone: its release check marked the screen lapped.
        #expect(ring.liveSpans == 0)
        #expect(hooks.requests.withLock { $0 }.isEmpty)
        let next = decoder.admit(Self.advance(to: 1700), decoder: wire)
        guard case .resync(.gap) = next else {
            Issue.record("the frame after a lapped parse gave \(next)")
            return
        }
        deliver(Self.ready(offset: 1700, generation: 2), &decoder, queue, hooks)
        ring.write(Self.bytes(0x64, count: 64))
        deliver(Self.advance(to: 1764), &decoder, queue, hooks)
        queue.finish()
        #expect(await Self.drain(queue) == [
            .snapshot(Self.readyFrame(offset: 1700, generation: 2)),
            .output(Data(Self.bytes(0x64, count: 64)), colors: nil),
        ])
    }

    /// A span the writer lapped while it waited in the queue never reaches the view; the stream resyncs.
    @Test func aSpanLappedInTheQueueIsDroppedUnparsed() async throws {
        let ring = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: ring.mapper)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        ring.write(Self.bytes(0x61, count: 600))
        deliver(Self.advance(to: 600), &decoder, queue, hooks)  // queued behind the READY
        ring.write(Self.bytes(0x62, count: 600))
        ring.write(Self.bytes(0x63, count: 500))  // laps 0 ..< 600 before the view takes it
        queue.finish()

        #expect(await Self.drain(queue) == [.snapshot(Self.readyFrame(offset: 0, generation: 1))])
        let next = decoder.admit(Self.advance(to: 1700), decoder: wire)
        guard case .resync(.gap) = next else {
            Issue.record("the frame after a lapped span gave \(next)")
            return
        }
    }

    // MARK: Lifetime

    /// The stream holds one reference to the mapping and every span the view holds one more: the mapping goes
    /// with the last of them, never under a span being parsed.
    @Test func theMappingOutlivesTheStreamUntilTheLastSpanGoes() async throws {
        weak var mapped: InProcessRing?
        var held: Data?
        do {
            let ring = InProcessRing(capacity: 1024)
            mapped = ring
            let hooks = RingHooks()
            let queue = TerminalEventQueue(recorder: nil)
            var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: ring.mapper)
            deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
            deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
            ring.write(Self.bytes(0x61, count: 300))
            deliver(Self.advance(to: 300), &decoder, queue, hooks)
            _ = await queue.next()
            guard case .output(let data, nil)? = await queue.next() else {
                Issue.record("no output")
                return
            }
            held = data
        }
        #expect(mapped != nil)  // the stream is gone; the span the view holds keeps the mapping
        #expect(held?.count == 300)
        held = nil
        #expect(mapped == nil)
    }

    // MARK: Protocol errors

    /// A RING or RING_ADVANCE the stream cannot use closes the connection (`bad_message`) and ends the stream
    /// with `connectionLost`, so the view reattaches on the next connection; never a silent fallback.
    @Test func ringFramesTheStreamCannotUseCloseTheConnection() async throws {
        let ring = InProcessRing(capacity: 1024)
        let cases: [(frames: [TerminalAttachFrame], mapper: TerminalRingMapper?, reason: String)] = [
            ([Self.ringFrame(ring, end: 0)], nil, "ring_not_negotiated"),
            ([Self.ringFrame(ring, end: 0, descriptor: false)], ring.mapper, "ring_without_descriptor"),
            ([Self.ringFrame(meta: #"{"capacity":1024,"base":0,"layout":2}"#)], ring.mapper, "ring_layout_2"),
            ([Self.ringFrame(meta: #"{"capacity":0,"base":0,"layout":1}"#)], ring.mapper, "ring_capacity_0"),
            ([Self.ringFrame(meta: #"{"capacity":134217728,"base":0,"layout":1}"#)], ring.mapper,
             "ring_capacity_134217728"),
            ([Self.ringFrame(meta: #"{"capacity":"big"}"#)], ring.mapper, "ring_bad_meta"),
            ([Self.ringFrame(ring, end: 0)], InProcessRing.refusingMapper, "ring_mapping_writable"),
            ([Self.advance(to: 10)], ring.mapper, "ring_advance_without_ring"),
            ([Self.ringFrame(ring, end: 0),
              TerminalAttachFrame(kind: .ringAdvance, offset: 10, payload: TerminalFrameBytes(Data([1])))],
             ring.mapper, "ring_advance_without_ring"),
        ]
        for (frames, mapper, reason) in cases {
            let hooks = RingHooks()
            let queue = TerminalEventQueue(recorder: nil)
            var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: mapper)
            for frame in frames { deliver(frame, &decoder, queue, hooks) }
            queue.finish()
            #expect(hooks.closes.withLock { $0 } == [reason])
            #expect(await Self.drain(queue) == [.closed(.connectionLost("bad_message: \(reason)"))])
        }
    }

    @Test func aMappedRingTakesTheFramesDescriptor() throws {
        let ring = InProcessRing(capacity: 1024)
        let frame = Self.ringFrame(ring, end: 0)
        var decoder = TerminalAttachFrameDecoder(surface: Self.surface, ringMapper: ring.mapper)
        guard case .nothing = decoder.admit(frame, decoder: wire) else {
            Issue.record("a good RING was refused")
            return
        }
        #expect(frame.descriptor?.take() == nil)  // the mapper owns it and closed it
        #expect(ring.descriptorsMapped == 1)
    }

    // MARK: Helpers

    private func deliver(_ frame: TerminalAttachFrame, _ decoder: inout TerminalAttachFrameDecoder,
                         _ queue: TerminalEventQueue, _ hooks: RingHooks) {
        TerminalAttachFrameDecoder.deliver(decoder.admit(frame, decoder: wire), to: queue, delivery: hooks.delivery)
    }

    private static func drain(_ queue: TerminalEventQueue) async -> [TerminalChannelEvent] {
        var events: [TerminalChannelEvent] = []
        while let event = await queue.next() { events.append(event) }
        return events
    }

    private static func advance(to end: UInt64) -> TerminalAttachFrame {
        TerminalAttachFrame(kind: .ringAdvance, offset: end, payload: TerminalFrameBytes(Data()))
    }

    /// A RING frame for `ring`, with a descriptor the test mapper closes.
    private static func ringFrame(_ ring: InProcessRing, end: UInt64, descriptor: Bool = true) -> TerminalAttachFrame {
        ringFrame(meta: #"{"capacity":\#(ring.capacity),"base":0,"layout":1}"#, end: end, descriptor: descriptor)
    }

    private static func ringFrame(meta: String, end: UInt64 = 0, descriptor: Bool = true) -> TerminalAttachFrame {
        TerminalAttachFrame(kind: .ring, offset: end, payload: TerminalFrameBytes(Data(meta.utf8)),
                            descriptor: descriptor ? TerminalRingDescriptor(open("/dev/null", O_RDONLY)) : nil)
    }

    /// A READY SNAPSHOT frame, `meta_len | meta | data`.
    private static func ready(offset: UInt64, generation: UInt64) -> TerminalAttachFrame {
        let data = Array("SCREEN-\(generation)".utf8)
        let meta = Array(#"{"event":"snapshot","surface":3,"phase":"ready","generation":\#(generation),"offset":\#(offset),"version":1,"cols":80,"rows":24,"data_len":\#(data.count)}"#.utf8)
        let length = (0 ..< 4).map { UInt8(truncatingIfNeeded: meta.count >> (8 * $0)) }
        return TerminalAttachFrame(kind: .snapshot, offset: offset, payload: TerminalFrameBytes(Data(length + meta + data)))
    }

    private static func readyFrame(offset: UInt64, generation: UInt64) -> TerminalSnapshotFrame {
        TerminalSnapshotFrame(phase: .ready, generation: generation, offset: offset, version: 1, cols: 80, rows: 24,
                              data: Data("SCREEN-\(generation)".utf8))
    }

    private static func bytes(_ value: UInt8, count: Int) -> [UInt8] { [UInt8](repeating: value, count: count) }

    private static func address(_ data: Data) -> UInt {
        // SAFETY: only the address is read, inside the closure that lends it.
        unsafe data.withUnsafeBytes { buffer in unsafe UInt(bitPattern: buffer.baseAddress) }
    }
}

/// An output ring in this process: a shared anonymous mapping the test writes as the daemon's writer does
/// (`reserve`, copy, `end`; app-daemon-connection-DESIGN.md section 4.2), read through ``TerminalOutputRing``.
private final class InProcessRing: TerminalOutputRing {
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
        bytes.withUnsafeBytes { source in
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

    func reserveAfterReading() -> UInt64 {
        reserve.load(ordering: .acquiring)
    }

    func bytesNoCopy(from start: UInt64, count: Int, released: @escaping @Sendable () -> Void) -> Data {
        spans.add(1, ordering: .relaxed)
        // SAFETY: `count` bytes from `start % capacity` lie in the data area (the protocol's contract), and the
        // deallocator holds the ring, so the mapping outlives the Data.
        let pointer = unsafe UnsafeMutableRawPointer(bitPattern: address + UInt(start % capacity))!
        return unsafe Data(bytesNoCopy: pointer, count: count, deallocator: .custom { _, _ in
            self.spans.add(-1, ordering: .relaxed)
            released()
        })
    }
}

/// What the decoder asked the attachment for.
private final class RingHooks: Sendable {
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
