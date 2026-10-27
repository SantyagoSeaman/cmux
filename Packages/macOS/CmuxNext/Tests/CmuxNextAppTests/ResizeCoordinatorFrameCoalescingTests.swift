import CmuxNextBridge
import struct CmuxNextDaemon.CellSize
import Testing

/// The app's continuous input to the daemon is coalesced per refresh where it is made (p05-ipc-141): a terminal's
/// grid reports of one frame reach its PTY owner once, the newest, at the frame tick.
@MainActor
@Suite struct ResizeCoordinatorFrameCoalescingTests {
    final class Terminal: ResizeTarget {
        var applied: [CellSize] = []
        var awaitsEcho = false
        func applySettled(_ size: CellSize) { applied.append(size) }
    }

    @Test func aHundredResizesInAFrameSendOne() {
        let coordinator = ResizeCoordinator(holds: { _ in false })
        let terminal = Terminal(), other = Terminal()
        for cols in 1 ... 100 {
            coordinator.submit(terminal, size: CellSize(cols: cols, rows: 40))
            coordinator.submit(other, size: CellSize(cols: 200 - cols, rows: 30))
        }
        #expect(terminal.applied.isEmpty && other.applied.isEmpty, "a grid went before the frame")
        _ = coordinator.tick()
        #expect(terminal.applied == [CellSize(cols: 100, rows: 40)], "sent \(terminal.applied)")
        #expect(other.applied == [CellSize(cols: 100, rows: 30)], "sent \(other.applied)")
    }
}
