@testable import CmuxNextDaemon
import CmuxNextIPC
import Foundation
import Testing

/// Continuous requests on a ``LineTransport`` (p05-ipc-141): while the daemon does not read, a newer grid report
/// replaces the unsent one, and the replaced request leaves the pending requests, since nothing will answer its id.
@Suite(.serialized, .timeLimit(.minutes(1))) struct LineTransportCoalescingTests {
    typealias FakeDaemon = ReplyEventOrderTests.FakeDaemon

    private static let key = FrameWriter.CoalesceKey(method: MethodID(rawValue: 0x0102), target: 7, stream: 0)

    private static func resize(_ id: UInt64, cols: Int) -> Data {
        Data(#"{"id":\#(id),"cmd":"resize-attached-view","surface":7,"lease":"l","cols":\#(cols),"rows":24}"#.utf8)
    }

    @Test func aCoalescedRequestLeavesNoWaiterBehind() throws {
        let daemon = try FakeDaemon()
        let transport = try LineTransport(path: daemon.path, role: .control, stats: nil)
        try daemon.accept()
        transport.start(onEvent: { _, _, _ in }, onClose: { _ in })
        defer { transport.close() }

        // 4 MiB with no id: more than both 512 KiB socket buffers hold, so the daemon, which reads nothing, leaves
        // the writer inside this send while the view resizes.
        #expect(transport.sendLine(Data(#"{"pad":"\#(String(repeating: "a", count: 4 << 20))"}"#.utf8)))
        for cols in 1 ... 100 {
            try transport.sendNoReply(cmd: "resize-attached-view", coalescing: Self.key) { id in
                Self.resize(id, cols: cols)
            }
        }
        #expect(transport.pendingRequests == 1, "a replaced request still waits for a reply that will never come")

        // An uncoalesced request still waits for its reply.
        try transport.sendNoReply(cmd: "resize-attached-view") { id in Self.resize(id, cols: 200) }
        #expect(transport.pendingRequests == 2)
    }
}
