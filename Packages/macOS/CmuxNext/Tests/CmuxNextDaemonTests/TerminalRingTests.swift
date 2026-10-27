import CmuxNextCompat
import Darwin
import Foundation
import Testing
@testable import CmuxNextDaemon

/// Ring streams (terminal-output-ring-v1, p05-ipc-113): a RING maps the terminal's output ring, a RING_ADVANCE
/// gives at most two spans that reach the view as `Data` pointing into the mapping (no copy), spans merge only
/// with the adjacent span of the same mapping, and a gap or a lap, at cut, in the queue or during the parse,
/// asks for a fresh READY exactly once, at once, whether or not another frame follows.
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
        var decoder = Self.decoder(ring, queue, hooks)
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
        var decoder = Self.decoder(ring, queue, hooks)
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
        #expect([head.range, wrapped.range] == [1000 ..< 1024, 1024 ..< 1300])
        TerminalAttachFrameDecoder.deliver(wrapping, to: queue, delivery: hooks.delivery)
        ring.write(Self.bytes(0x64, count: 100))  // 1300 ..< 1400 continues the wrapped span in memory
        deliver(Self.advance(to: 1400), &decoder, queue, hooks)
        #expect(queue.queuedItemCount == 2)  // merged at the queue's tail: 700 ..< 1024 and 1024 ..< 1400
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
        var decoder = Self.decoder(ring, queue, hooks)
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
        var decoder = Self.decoder(ring, queue, hooks)
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

    /// The check at release: the writer lapped a span while the view parsed it, so the screen may be torn. The
    /// span's release asks for a fresh READY at once, with no frame after it; frames until the READY are dropped
    /// without a second request, and a late release of the replaced screen's span does not mark the new screen.
    @Test func aLapDuringTheParseAsksForTheFreshScreenAtRelease() async throws {
        let ring = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = Self.decoder(ring, queue, hooks)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        _ = await queue.next()  // the READY
        ring.write(Self.bytes(0x61, count: 600))
        deliver(Self.advance(to: 600), &decoder, queue, hooks)
        await Self.parse(queue) {
            // The view parses 0 ..< 600 while the writer laps it.
            ring.write(Self.bytes(0x62, count: 600))
            ring.write(Self.bytes(0x63, count: 500))
        }
        // The parsed span is gone: its release check asked, and no frame came after it.
        #expect(ring.liveSpans == 0)
        #expect(hooks.requests.withLock { $0 } == [.gap])

        let next = decoder.admit(Self.advance(to: 1700), decoder: wire)
        guard case .nothing = next else {
            Issue.record("a frame while the fresh READY is due gave \(next)")
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
        #expect(hooks.requests.withLock { $0 } == [.gap])
    }

    /// A span the writer lapped while it waited in the queue never reaches the view, and the consumer's step
    /// that finds it asks for a fresh READY with no frame after it.
    @Test func aSpanLappedInTheQueueAsksAtThePopWithNoFrameAfter() async throws {
        let ring = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = Self.decoder(ring, queue, hooks)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        ring.write(Self.bytes(0x61, count: 600))
        deliver(Self.advance(to: 600), &decoder, queue, hooks)  // queued behind the READY
        ring.write(Self.bytes(0x62, count: 600))
        ring.write(Self.bytes(0x63, count: 500))  // laps 0 ..< 600 before the view takes it; no advance follows
        queue.finish()

        #expect(await Self.drain(queue) == [.snapshot(Self.readyFrame(offset: 0, generation: 1))])
        #expect(hooks.requests.withLock { $0 } == [.gap])
        let next = decoder.admit(Self.advance(to: 1700), decoder: wire)
        guard case .nothing = next else {
            Issue.record("the frame after a claimed lap gave \(next)")
            return
        }
    }

    /// A flood while the consumer is behind: spans 0 ..< 600, 600 ..< 1100 and 1100 ..< 1700 in a 1 KiB ring.
    /// The push of 600 ..< 1100 proves 0 ..< 600 lapped: the stream asks once, at that push, the backlog goes,
    /// and no span is left waiting for a frame that never comes.
    @Test func aLapInTheQueueAsksAtThePushThatProvesIt() async throws {
        let ring = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = Self.decoder(ring, queue, hooks)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        for (value, end) in [(UInt8(0x61), UInt64(600)), (0x62, 1100), (0x63, 1700)] {
            ring.write(Self.bytes(value, count: Int(end - ring.end)))
            deliver(Self.advance(to: end), &decoder, queue, hooks)
        }

        #expect(hooks.requests.withLock { $0 } == [.gap])
        #expect(queue.queuedItemCount == 1)  // the READY
        queue.finish()
        #expect(await Self.drain(queue) == [.snapshot(Self.readyFrame(offset: 0, generation: 1))])
    }

    /// A consumer stalled during a flood: ten thousand advances leave a bounded queue. Spans that continue each
    /// other join the queue's last run, and once the writer laps the oldest run the backlog goes with one request.
    @Test func tenThousandAdvancesWithNoConsumerStayBounded() async throws {
        let ring = InProcessRing(capacity: 1 << 20)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = Self.decoder(ring, queue, hooks)
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        var most = 0
        for index in 0 ..< 10_000 {
            ring.write(Self.bytes(UInt8(truncatingIfNeeded: index), count: 64))  // 640,000 bytes: no lap yet
            deliver(Self.advance(to: ring.end), &decoder, queue, hooks)
            most = max(most, queue.queuedItemCount)
        }
        #expect(most == 2)  // the READY and one run of 640,000 bytes
        #expect(hooks.requests.withLock { $0 }.isEmpty)

        for index in 0 ..< 10_000 {
            ring.write(Self.bytes(UInt8(truncatingIfNeeded: index), count: 64))  // laps the run
            deliver(Self.advance(to: ring.end), &decoder, queue, hooks)
            most = max(most, queue.queuedItemCount)
        }
        #expect(most <= 3)  // the run is split once, at the wrap
        #expect(queue.queuedItemCount == 1)
        #expect(hooks.requests.withLock { $0 } == [.gap])
    }

    /// A byte-replay stream (no READY to resync with) answers a lap by reattaching, as on the daemon's `overflow`.
    @Test func aLapOnAByteReplayStreamReattaches() async throws {
        let ring = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = Self.decoder(ring, queue, hooks)
        let vtState = #"{"event":"vt-state","surface":3,"cols":80,"rows":24,"data":""}"#
        TerminalAttachFrameDecoder.deliver(decoder.admit(line: Data(vtState.utf8), named: "vt-state", decoder: wire),
                                           to: queue, delivery: hooks.delivery)
        deliver(Self.ringFrame(ring, end: 0), &decoder, queue, hooks)
        ring.write(Self.bytes(0x61, count: 600))
        deliver(Self.advance(to: 600), &decoder, queue, hooks)
        ring.write(Self.bytes(0x62, count: 600))
        ring.write(Self.bytes(0x63, count: 500))

        #expect(await queue.next() == .replay(TerminalReplay(cols: 80, rows: 24, data: Data())))
        #expect(await queue.next() == .closed(.overflow))
        #expect(await queue.next() == nil)
        #expect(hooks.requests.withLock { $0 }.isEmpty)
    }

    /// A second RING with no READY before it maps a new ring for the same screen: it keeps the screen's lap mark,
    /// so a lap claimed before it still holds the stream until the READY, and a span of the old mapping lapped
    /// after it still asks for the fresh screen.
    @Test func aSecondRingKeepsTheScreensLapMark() async throws {
        let first = InProcessRing(capacity: 1024)
        let second = InProcessRing(capacity: 1024)
        let hooks = RingHooks()
        let queue = TerminalEventQueue(recorder: nil)
        let mapper = RingSequence([first, second])
        var decoder = TerminalAttachFrameDecoder(
            surface: Self.surface, rings: Self.streams(mapper.mapper, queue, hooks))
        deliver(Self.ready(offset: 0, generation: 1), &decoder, queue, hooks)
        deliver(Self.ringFrame(first, end: 0), &decoder, queue, hooks)
        _ = await queue.next()  // the READY
        first.write(Self.bytes(0x61, count: 600))
        deliver(Self.advance(to: 600), &decoder, queue, hooks)
        do {
            guard case .output(let held, nil)? = await queue.next() else {
                Issue.record("no output")
                return
            }
            // The daemon replaces the ring (same screen and offsets); the view still parses the old mapping's span.
            second.write(Self.bytes(0x61, count: 600))
            deliver(Self.ringFrame(second, end: 600), &decoder, queue, hooks)
            withExtendedLifetime(held) {
                first.write(Self.bytes(0x62, count: 600))
                first.write(Self.bytes(0x63, count: 500))  // laps the old mapping's span during the parse
            }
        }
        #expect(hooks.requests.withLock { $0 } == [.gap])

        second.write(Self.bytes(0x64, count: 100))
        let next = decoder.admit(Self.advance(to: 700), decoder: wire)
        guard case .nothing = next else {
            Issue.record("an advance on the replaced ring while the READY is due gave \(next)")
            return
        }
        deliver(Self.ready(offset: 700, generation: 2), &decoder, queue, hooks)
        second.write(Self.bytes(0x65, count: 10))
        deliver(Self.advance(to: 710), &decoder, queue, hooks)
        queue.finish()
        #expect(await Self.drain(queue) == [
            .snapshot(Self.readyFrame(offset: 700, generation: 2)),
            .output(Data(Self.bytes(0x65, count: 10)), colors: nil),
        ])
        #expect(hooks.requests.withLock { $0 } == [.gap])
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
            var decoder = Self.decoder(ring, queue, hooks)
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
            // The ring has published nothing: bytes up to 10 would be an earlier lap's.
            ([Self.ringFrame(ring, end: 0), Self.advance(to: 10)], ring.mapper, "ring_advance_unpublished"),
        ]
        for (frames, mapper, reason) in cases {
            let hooks = RingHooks()
            let queue = TerminalEventQueue(recorder: nil)
            var decoder = TerminalAttachFrameDecoder(
                surface: Self.surface, rings: mapper.map { Self.streams($0, queue, hooks) })
            for frame in frames { deliver(frame, &decoder, queue, hooks) }
            queue.finish()
            #expect(hooks.closes.withLock { $0 } == [reason])
            #expect(await Self.drain(queue) == [.closed(.connectionLost("bad_message: \(reason)"))])
        }
    }

    @Test func aMappedRingTakesTheFramesDescriptor() throws {
        let ring = InProcessRing(capacity: 1024)
        let frame = Self.ringFrame(ring, end: 0)
        let queue = TerminalEventQueue(recorder: nil)
        var decoder = Self.decoder(ring, queue, RingHooks())
        guard case .nothing = decoder.admit(frame, decoder: wire) else {
            Issue.record("a good RING was refused")
            return
        }
        #expect(frame.descriptor?.take() == nil)  // the mapper owns it and closed it
        #expect(ring.descriptorsMapped == 1)
    }

    // MARK: Helpers

    private static func decoder(_ ring: InProcessRing, _ queue: TerminalEventQueue,
                                _ hooks: RingHooks) -> TerminalAttachFrameDecoder {
        TerminalAttachFrameDecoder(surface: surface, rings: streams(ring.mapper, queue, hooks))
    }

    private static func streams(_ mapper: @escaping TerminalRingMapper, _ queue: TerminalEventQueue,
                                _ hooks: RingHooks) -> TerminalAttachFrameDecoder.RingStreams {
        TerminalAttachFrameDecoder.RingStreams(mapper: mapper, queue: queue, delivery: hooks.delivery)
    }

    private func deliver(_ frame: TerminalAttachFrame, _ decoder: inout TerminalAttachFrameDecoder,
                         _ queue: TerminalEventQueue, _ hooks: RingHooks) {
        TerminalAttachFrameDecoder.deliver(decoder.admit(frame, decoder: wire), to: queue, delivery: hooks.delivery)
    }

    /// Takes the next output and holds it while `during` runs, as the lane does while Ghostty parses it; the
    /// output is released when this returns.
    private static func parse(_ queue: TerminalEventQueue, during: () -> Void) async {
        guard case .output(let parsing, nil)? = await queue.next() else {
            Issue.record("no output")
            return
        }
        withExtendedLifetime(parsing, during)
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
