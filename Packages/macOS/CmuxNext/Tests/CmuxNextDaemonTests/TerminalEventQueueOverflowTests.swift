import Foundation
import Testing
@testable import CmuxNextDaemon

/// Each view's stream has its own bounded queue (p01-process-127): the reader
/// thread serves every stream of its connection, so a push never waits, and a
/// view that falls behind drops only its own output (D5).
@Suite struct TerminalEventQueueOverflowTests {
    @Test func aQueueDropsItsOutputButKeepsItsEvents() async {
        let queue = TerminalEventQueue(highWater: 8)
        #expect(queue.push(.output(Data("12345".utf8), colors: nil)) == .queued)
        #expect(queue.push(.scrollChanged(offset: 3, atBottom: true)) == .queued)
        // Past the high water mark: returns at once (never waits) and drops output.
        #expect(queue.push(.output(Data("67890".utf8), colors: nil)) == .overflowed)
        #expect(queue.bufferedOutputBytes == 0)
        #expect(queue.push(.output(Data("ok".utf8), colors: nil)) == .queued)
        queue.finish()
        var events: [TerminalChannelEvent] = []
        while let event = await queue.next() { events.append(event) }
        #expect(events == [.scrollChanged(offset: 3, atBottom: true), .output(Data("ok".utf8), colors: nil)])
    }

    /// One stream's overflow leaves another stream's queue whole: the queues are per view, not per connection.
    @Test func oneStreamsOverflowLeavesAnotherStreamsOutput() async {
        let slow = TerminalEventQueue(highWater: 8)
        let fast = TerminalEventQueue(highWater: 8)
        #expect(fast.push(.output(Data("abc".utf8), colors: nil)) == .queued)
        #expect(slow.push(.output(Data("12345".utf8), colors: nil)) == .queued)
        #expect(slow.push(.output(Data("67890".utf8), colors: nil)) == .overflowed)
        #expect(slow.bufferedOutputBytes == 0)
        #expect(fast.bufferedOutputBytes == 3)
        #expect(await fast.next() == .output(Data("abc".utf8), colors: nil))
    }

    /// A push far past the mark still returns at once: no push ever parks the connection's reader.
    @Test func aPushNeverWaitsForTheConsumer() {
        let queue = TerminalEventQueue(highWater: 4)
        var overflows = 0
        for _ in 0..<1_000 where queue.push(.output(Data("12345".utf8), colors: nil)) == .overflowed {
            overflows += 1
        }
        #expect(overflows == 1_000)
        #expect(queue.bufferedOutputBytes == 0)
    }

    /// Chunks stay their own events: a ring span reaches the view as it was cut, never merged into a copy.
    @Test func chunksAreNeverMerged() async {
        let queue = TerminalEventQueue()
        queue.push(.output(Data("a".utf8), colors: nil))
        queue.push(.output(Data("b".utf8), colors: nil))
        #expect(await queue.next() == .output(Data("a".utf8), colors: nil))
        #expect(await queue.next() == .output(Data("b".utf8), colors: nil))
    }

    /// A READY is the screen a resync waits for: it never counts toward the mark, so it is never dropped.
    @Test func aReadyIsNeverDropped() async {
        let queue = TerminalEventQueue(highWater: 4)
        let ready = TerminalSnapshotFrame(phase: .ready, generation: 1, offset: 0, version: 1, data: Data(repeating: 1, count: 64))
        #expect(queue.push(.snapshot(ready)) == .queued)
        #expect(queue.bufferedOutputBytes == 0)
        #expect(await queue.next() == .snapshot(ready))
    }

    /// A consumer already waiting takes the pushed event directly: nothing is queued.
    @Test func aWaitingConsumerTakesThePushDirectly() async {
        let queue = TerminalEventQueue()
        let read = Task { await queue.next() }
        let chunk = TerminalChannelEvent.output(Data("x".utf8), colors: nil)
        // The consumer parks before the push or finds the event queued: either way it gets this chunk.
        queue.push(chunk)
        #expect(await read.value == chunk)
        #expect(queue.bufferedOutputBytes == 0)
    }
}
