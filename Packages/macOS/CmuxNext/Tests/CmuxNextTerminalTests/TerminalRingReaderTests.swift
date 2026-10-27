import CmuxNextCompat
@testable import CmuxNextTerminal
import Foundation
import GhosttyNextKit
import Testing

/// A surface's output lane reads ring output by copy, then validate (p01-process-131): one copy of
/// `[start, end)` out of the daemon's ring right before the parse, then the ring's oldest intact offset
/// again; a chunk the writer overwrote before or during the copy is never parsed, its stream is told once
/// (the snapshot-request) and the chunks cut before that READY are dropped. The lane's parse is the test's
/// recorder and the ring is ``TestOutputRing``, a writer with output_ring.rs's protocol in this process.
@Suite(.timeLimit(.minutes(1))) struct TerminalRingReaderTests {
    static var surface: ghostty_surface_t { unsafeBitCast(UInt(0x1000), to: ghostty_surface_t.self) }

    private func lane(_ label: String, log: ParsedLog, highWater: Int = 2 << 20) -> TerminalOutputLane {
        TerminalOutputLane(surface: Self.surface, label: label, highWater: highWater) { _, data in log.append(data) }
    }

    @Test func aChunkIsCopiedOnceThenParsed() async {
        let ring = TestOutputRing(capacity: 64), log = ParsedLog()
        let lane = lane("test.ring.once", log: log)
        defer { lane.close() }
        ring.push(Array("hello".utf8))

        #expect(await lane.takeOffMain(.ringOutput(ring.chunk(0, 5))))
        await lane.drained()

        #expect(log.chunks == [Data("hello".utf8)])
        #expect(lane.ringReads == .init(chunks: 1, copies: 1, bytesCopied: 5, laps: 0, dropped: 0))
        #expect(ring.lappedChunks.isEmpty)
    }

    /// The copy is the chunk's own bytes, never a view of the ring: a keystroke echo (inline in its Data), a
    /// longer chunk and a wrapped one keep their bytes after the writer overwrites the ring.
    @Test func aReadOwnsItsBytesWhateverTheChunkSize() {
        let ring = TestOutputRing(capacity: 64)
        ring.push(Array("x".utf8))
        ring.push(Array("0123456789abcdefghijklmnopqrstuv".utf8))  // offsets 1..<33
        var reads = [ring.chunk(0, 1).read(), ring.chunk(1, 33).read()]
        ring.push([UInt8](repeating: 0x2E, count: 40))  // offsets 33..<73: wraps to data positions 0..<9
        reads.append(ring.chunk(33, 73).read())
        ring.push([UInt8](repeating: 0x21, count: 64))  // the writer moves on: every byte above is overwritten

        #expect(reads == [.copied(Data("x".utf8)), .copied(Data("0123456789abcdefghijklmnopqrstuv".utf8)),
                          .copied(Data(repeating: 0x2E, count: 40))])
    }

    /// `debug.terminal` sums the live lanes' counts and reports copies per chunk and the ring path's zero
    /// base64 and JSON decodes.
    @Test func debugTerminalSumsTheLanes() throws {
        let report = TerminalRingReadsReport([
            TerminalRingReads(chunks: 3, copies: 1, bytesCopied: 5, laps: 1, dropped: 1),
            TerminalRingReads(chunks: 1, copies: 1, bytesCopied: 22, laps: 0, dropped: 0),
        ])
        #expect(report.reads == .init(chunks: 4, copies: 2, bytesCopied: 27, laps: 1, dropped: 1))

        let reply = report.foundationObject
        let ring = try #require(reply["ring"] as? [String: Double])
        #expect(reply["lanes"] as? Double == 2)
        #expect(ring == ["chunks": 4, "copies": 2, "bytes_copied": 27, "laps": 1, "dropped": 1,
                         "copies_per_chunk": 0.5, "base64_decodes": 0, "json_decodes": 0])
        #expect(TerminalRingReadsReport([]).foundationObject["ring"] as? [String: Double]
            == ["chunks": 0, "copies": 0, "bytes_copied": 0, "laps": 0, "dropped": 0, "copies_per_chunk": 0,
                "base64_decodes": 0, "json_decodes": 0])
    }

    /// A chunk across the ring's end is still one copy: two spans into one buffer.
    @Test func aWrappedChunkIsOneCopyOfBothSpans() async {
        let ring = TestOutputRing(capacity: 16), log = ParsedLog()
        let lane = lane("test.ring.wrap", log: log)
        defer { lane.close() }
        ring.push(Array("abcdefghijkl".utf8))
        #expect(await lane.takeOffMain(.ringOutput(ring.chunk(0, 12))))
        await lane.drained()
        ring.push(Array("mnopqrstuv".utf8))  // offsets 12..<22: data positions 12..<16, then 0..<6

        #expect(await lane.takeOffMain(.ringOutput(ring.chunk(12, 22))))
        await lane.drained()

        #expect(log.chunks == [Data("abcdefghijkl".utf8), Data("mnopqrstuv".utf8)])
        #expect(lane.ringReads == .init(chunks: 2, copies: 2, bytesCopied: 22, laps: 0, dropped: 0))
    }

    /// The writer overwrote the chunk while it waited on the lane: nothing is copied or parsed and the stream
    /// is told once.
    @Test func aChunkLappedBeforeTheReadIsNeverParsed() async {
        let ring = TestOutputRing(capacity: 64), log = ParsedLog()
        let lane = lane("test.ring.lapped", log: log)
        defer { lane.close() }
        ring.push(Array("hello".utf8))
        let chunk = ring.chunk(0, 5)
        ring.push([UInt8](repeating: 0x78, count: 64))

        #expect(await lane.takeOffMain(.ringOutput(chunk)))
        await lane.drained()

        #expect(log.chunks.isEmpty)
        #expect(ring.lappedChunks == [chunk])
        #expect(lane.ringReads == .init(chunks: 1, copies: 0, bytesCopied: 0, laps: 1, dropped: 0))
    }

    /// The writer overwrites the chunk during the copy (between the two oldest reads): the torn copy is dropped
    /// whole, never parsed.
    @Test func aCopyTornByTheWriterIsDroppedWhole() async {
        let ring = TestOutputRing(capacity: 64), log = ParsedLog()
        let lane = lane("test.ring.torn", log: log)
        defer { lane.close() }
        ring.push(Array("hello".utf8))
        ring.beforeOldestRead = { read in
            // The second read is the check after the copy: the writer ran a whole lap during the copy.
            if read == 2 { ring.push([UInt8](repeating: 0x78, count: 64)) }
        }

        #expect(await lane.takeOffMain(.ringOutput(ring.chunk(0, 5))))
        await lane.drained()

        #expect(log.chunks.isEmpty)
        #expect(ring.lappedChunks == [ring.chunk(0, 5)])
        #expect(lane.ringReads == .init(chunks: 1, copies: 0, bytesCopied: 0, laps: 1, dropped: 0))
    }

    /// After a lap the lane drops the chunks cut before the READY that answers it (they follow a gap), even
    /// intact ones, and parses those cut after it.
    @Test func chunksCutBeforeTheResyncAreDropped() async {
        let ring = TestOutputRing(capacity: 64), log = ParsedLog()
        let lane = lane("test.ring.resync", log: log)
        defer { lane.close() }
        ring.push(Array("hello".utf8))
        ring.push([UInt8](repeating: 0x78, count: 64))  // laps [0, 5)
        let lapped = ring.chunk(0, 5), afterGap = ring.chunk(5, 69)
        ring.push(Array("world".utf8))

        #expect(await lane.takeOffMain(.ringOutput(lapped)))
        #expect(await lane.takeOffMain(.ringOutput(afterGap)))
        #expect(await lane.takeOffMain(.ringOutput(ring.chunk(69, 74, resync: 1))))
        await lane.drained()

        #expect(log.chunks == [Data("world".utf8)])
        #expect(ring.lappedChunks == [lapped])
        #expect(lane.ringReads == .init(chunks: 3, copies: 1, bytesCopied: 5, laps: 1, dropped: 1))
    }

    /// The lap flood: the writer runs while the lane copies, through a ring much smaller than the backlog, with
    /// stalls longer than a lap. Every parsed byte is the byte the writer wrote at its offset (each 8-byte word
    /// holds its own offset), every lap gives exactly one snapshot-request, and every chunk is copied at most once.
    @Test func aLapFloodParsesNoTornByteAndAsksOnceForEachLap() async {
        let ring = TestOutputRing(capacity: 4096)
        let checker = OffsetWordChecker()
        let lane = TerminalOutputLane(surface: Self.surface, label: "test.ring.flood", highWater: 64 << 10) { _, data in
            checker.check(data)
        }
        defer { lane.close() }
        var last: UInt64 = 0, resync: UInt64 = 0, emitted: UInt64 = 0
        for round in 0..<3_000 {
            let asked = UInt64(ring.lappedChunks.count)
            if asked > resync {
                // The stream's answer to the snapshot-request: a READY at the ring's end, then chunks cut with
                // the next resync count.
                resync = asked
                last = ring.end
                checker.ready(at: last)
            }
            // A stall: the app's reader fell more than a ring behind the writer.
            let words = round % 500 == 499 ? 768 : 1 + round % 97
            ring.pushOffsetWords(words)
            let end = ring.end
            #expect(await lane.takeOffMain(.ringOutput(ring.chunk(last, end, resync: resync))))
            emitted += 1
            last = end
            if round % 64 == 63 { await lane.drained() }
        }
        await lane.drained()

        let reads = lane.ringReads
        #expect(checker.mismatches == 0)
        #expect(reads.laps > 0)
        #expect(reads.laps == UInt64(ring.lappedChunks.count), "a lap without its one snapshot-request, or two")
        #expect(reads.chunks == emitted)
        #expect(reads.copies == checker.chunks, "a chunk copied but not parsed, or parsed twice")
        #expect(reads.copies + reads.laps + reads.dropped == reads.chunks)
    }

    /// 10,000 lines through the ring parse to the same bytes as through the frames path, one copy each.
    @Test func tenThousandLinesThroughTheRingParseAsTheFramesPathDoes() async {
        let lines = (0..<10_000).map { Data("line \($0) \(String(repeating: "x", count: $0 % 80))\r\n".utf8) }
        let framesLog = ParsedLog(), ringLog = ParsedLog()
        let frames = lane("test.ring.frames-path", log: framesLog)
        let ringLane = lane("test.ring.ring-path", log: ringLog)
        defer {
            frames.close()
            ringLane.close()
        }
        let ring = TestOutputRing(capacity: 1 << 20)
        for line in lines {
            #expect(await frames.takeOffMain(.output(line)))
            let start = ring.end
            ring.push(Array(line))
            #expect(await ringLane.takeOffMain(.ringOutput(ring.chunk(start, ring.end))))
        }
        await frames.drained()
        await ringLane.drained()

        let expected = lines.reduce(into: Data()) { $0.append($1) }
        #expect(framesLog.joined == expected)
        #expect(ringLog.joined == framesLog.joined)
        #expect(readEveryChunkOnce(ringLane, chunks: 10_000, bytes: UInt64(expected.count)))
    }

    private func readEveryChunkOnce(_ lane: TerminalOutputLane, chunks: UInt64, bytes: UInt64) -> Bool {
        lane.ringReads == .init(chunks: chunks, copies: chunks, bytesCopied: bytes, laps: 0, dropped: 0)
    }
}

/// Chunks a lane parsed, in order.
private nonisolated final class ParsedLog: Sendable {
    private let items = Mutex<[Data]>([])

    func append(_ data: Data) {
        items.withLock { $0.append(data) }
    }

    var chunks: [Data] { items.withLock { $0 } }
    var joined: Data { items.withLock { $0.reduce(into: Data()) { $0.append($1) } } }
}

/// Checks parsed flood chunks: each 8-byte word holds its own stream offset, a chunk continues the one before it
/// or starts at a READY, so a byte from another lap (a torn copy) is a mismatch.
private nonisolated final class OffsetWordChecker: Sendable {
    private struct State {
        var next: UInt64?
        var readyOffsets: Set<UInt64> = [0]
        var mismatches = 0
        var chunks: UInt64 = 0
    }

    private let state = Mutex(State())

    func ready(at offset: UInt64) {
        state.withLock { _ = $0.readyOffsets.insert(offset) }
    }

    func check(_ data: Data) {
        let words = stride(from: 0, to: data.count - data.count % 8, by: 8).map { at in
            data[data.startIndex + at ..< data.startIndex + at + 8].reduce(into: UInt64(0)) { word, byte in
                word = word >> 8 | UInt64(byte) << 56
            }
        }
        state.withLock { state in
            state.chunks += 1
            guard data.count % 8 == 0, let first = words.first else { return state.mismatches += 1 }
            if first != state.next, !state.readyOffsets.contains(first) { state.mismatches += 1 }
            for (index, word) in words.enumerated() where word != first + UInt64(index) * 8 {
                state.mismatches += 1
            }
            state.next = first + UInt64(words.count) * 8
        }
    }

    var mismatches: Int { state.withLock { $0.mismatches } }
    var chunks: UInt64 { state.withLock { $0.chunks } }
}

/// A `terminal-output-ring-v1` writer and its reader's view in this process, with output_ring.rs's protocol:
/// the writer stores `reserve`, fences, copies the bytes, then publishes `end`; the oldest intact offset is
/// `reserve - capacity` after an acquire fence.
private nonisolated final class TestOutputRing: TerminalOutputRing {
    let capacity: UInt64
    let base: UInt64 = 0
    /// The data area's address (a pointer is not `Sendable`); freed in deinit.
    private let address: UInt
    private let reserve = Atomic<UInt64>(0)
    private let published = Atomic<UInt64>(0)
    private let oldestReads = Atomic<Int>(0)
    private let laps = Mutex<[TerminalRingChunk]>([])
    private let hook = Mutex<(@Sendable (Int) -> Void)?>(nil)

    init(capacity: Int) {
        self.capacity = UInt64(capacity)
        // SAFETY: this ring's own data area, freed in deinit.
        address = unsafe UInt(bitPattern: UnsafeMutableRawPointer.allocate(byteCount: capacity, alignment: 8))
    }

    deinit {
        // SAFETY: the allocation from init.
        unsafe UnsafeMutableRawPointer(bitPattern: address)?.deallocate()
    }

    var end: UInt64 { published.load(ordering: .acquiring) }
    var lappedChunks: [TerminalRingChunk] { laps.withLock { $0 } }
    /// Runs before each oldest read with its number (1 is the read before the copy, 2 the one after).
    var beforeOldestRead: (@Sendable (Int) -> Void)? {
        get { hook.withLock { $0 } }
        set { hook.withLock { $0 = newValue } }
    }

    func chunk(_ start: UInt64, _ end: UInt64, resync: UInt64 = 0) -> TerminalRingChunk {
        TerminalRingChunk(ring: self, start: start, end: end, resync: resync)
    }

    // SAFETY: the ring's own `capacity` bytes, alive as long as the ring.
    var dataArea: UnsafeRawBufferPointer {
        unsafe UnsafeRawBufferPointer(start: UnsafeRawPointer(bitPattern: address), count: Int(capacity))
    }

    func oldestIntact() -> UInt64 {
        let read = oldestReads.add(1, ordering: .relaxed).newValue
        hook.withLock { $0 }?(read)
        atomicFence(ordering: .acquiring)
        let reserved = reserve.load(ordering: .relaxed)
        return reserved > capacity ? reserved - capacity : 0
    }

    func lapped(_ chunk: TerminalRingChunk) {
        laps.withLock { $0.append(chunk) }
    }

    /// `OutputRing::push`: one writer.
    func push(_ bytes: [UInt8]) {
        let start = published.load(ordering: .relaxed), target = start + UInt64(bytes.count)
        reserve.store(target, ordering: .relaxed)
        atomicFence(ordering: .releasing)
        for (index, byte) in bytes.enumerated() {
            let at = Int((start + UInt64(index)) % capacity)
            // SAFETY: `at` is inside the ring's own data area.
            unsafe UnsafeMutableRawPointer(bitPattern: address)?.storeBytes(of: byte, toByteOffset: at, as: UInt8.self)
        }
        published.store(target, ordering: .releasing)
    }

    /// `count` 8-byte little-endian words, each holding its own stream offset.
    func pushOffsetWords(_ count: Int) {
        let start = published.load(ordering: .relaxed)
        push((0..<UInt64(count)).flatMap { index in
            withUnsafeBytes(of: (start + index * 8).littleEndian) { Array($0) }
        })
    }
}
