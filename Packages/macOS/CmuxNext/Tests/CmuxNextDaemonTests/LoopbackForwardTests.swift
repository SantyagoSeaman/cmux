import CmuxNextCompat
@testable import CmuxNextDaemon
import Foundation
import Testing

/// A `loopback-forward-v1` stream's receive side (p05-ipc-149): OUTPUT frames carry the target's bytes raw at
/// contiguous offsets and give the same bytes as `loopback-data` lines; the credit queue holds at most the
/// window, and a gap, an overlap or bytes past the window end the stream.
@Suite(.timeLimit(.minutes(1))) struct LoopbackForwardTests {
    static let window = 16 * 1024

    /// The bytes the target sent, cut into frames of at most `frameBytes`.
    static func sent(_ count: Int) -> Data { Data((0..<count).map { UInt8(truncatingIfNeeded: $0 &* 31 &+ 7) }) }

    /// Reads data events until `count` bytes arrived, returning credit for each, as a consumer does.
    static func read(_ count: Int, from stream: LoopbackStream,
                     _ iterator: inout AsyncStream<LoopbackStreamEvent>.AsyncIterator) async -> Data {
        var received = Data()
        while received.count < count, let event = await iterator.next() {
            guard case .data(let bytes) = event else { break }
            received.append(bytes)
            stream.consumed(bytes.count)
        }
        return received
    }

    @Test func outputFramesGiveTheBytesOfTheLines() async {
        let bytes = Self.sent(5 * Self.window + 123)
        let lines = LoopbackStream(id: 1, receiveWindow: Self.window, sender: RecordingSender(), onFinish: { _ in })
        let frames = LoopbackStream(id: 2, receiveWindow: Self.window, sender: RecordingSender(), onFinish: { _ in })
        var fromLines = lines.events.makeAsyncIterator()
        var fromFrames = frames.events.makeAsyncIterator()
        var offset = 0
        while offset < bytes.count {
            let chunk = bytes.subdata(in: offset..<min(offset + Self.window / 4, bytes.count))
            let text = chunk.base64EncodedString()
            let line = Data(#"{"event":"loopback-data","stream":1,"data":"\#(text)"}"#.utf8)
            let event = LoopbackForwardEvent.decode(name: "loopback-data", line: line)
            #expect(event == .data(stream: 1, bytes: chunk))
            if let event { lines.deliver(event) }
            frames.deliverOutput(chunk, at: UInt64(offset))
            #expect(await Self.read(chunk.count, from: lines, &fromLines) == chunk)
            #expect(await Self.read(chunk.count, from: frames, &fromFrames) == chunk)
            offset += chunk.count
        }
        frames.close()
        lines.close()
    }

    @Test func aFrameAtTheWrongOffsetEndsTheStream() async {
        for wrong: UInt64 in [0, 50, 101] {
            let sender = RecordingSender()
            let stream = LoopbackStream(id: 7, receiveWindow: Self.window, sender: sender, onFinish: { _ in })
            stream.deliverOutput(Self.sent(100), at: 0)
            stream.deliverOutput(Self.sent(10), at: wrong)
            var events: [LoopbackStreamEvent] = []
            for await event in stream.events { events.append(event) }
            #expect(events.first == .data(Self.sent(100)))
            guard case .closed(let error) = events.last, case .some(.protocolViolation) = error else {
                Issue.record("a gap or overlap at \(wrong) did not end the stream: \(events)")
                continue
            }
            #expect(sender.lines.contains { $0.contains(#""cmd":"loopback-close""#) })
        }
    }

    @Test func theCreditQueueNeverHoldsMoreThanTheWindow() async {
        let stream = LoopbackStream(id: 3, receiveWindow: Self.window, sender: RecordingSender(), onFinish: { _ in })
        // One byte per frame fills the queue to its bound in events, then one byte more is past the window.
        for offset in 0..<Self.window { stream.deliverOutput(Data([UInt8(truncatingIfNeeded: offset)]), at: UInt64(offset)) }
        stream.deliverOutput(Data([0]), at: UInt64(Self.window))
        var data = 0
        var last: LoopbackStreamEvent?
        for await event in stream.events {
            if case .data(let bytes) = event { data += bytes.count }
            last = event
        }
        #expect(data == Self.window)
        #expect(last == .closed(error: .protocolViolation("the daemon sent past the window")))
        #expect(LoopbackStream.queueCapacity(receiveWindow: Self.window) == Self.window + 2)
    }

    @Test func noLoopbackSourceBuffersWithoutABound() throws {
        let folder = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/CmuxNextDaemon/Loopback")
        let files = try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "swift" }
        #expect(!files.isEmpty)
        for file in files {
            let source = try String(contentsOf: file, encoding: .utf8)
            #expect(!source.contains(".unbounded"), "\(file.lastPathComponent) buffers without a bound")
        }
    }
}

/// Records the lines a stream sends to its daemon.
private nonisolated final class RecordingSender: LoopbackLineSending {
    private let sent = Mutex<[String]>([])

    var lines: [String] { sent.withLock { $0 } }

    func send(_ line: Data) -> Bool {
        sent.withLock { $0.append(String(decoding: line, as: UTF8.self)) }
        return true
    }
}
