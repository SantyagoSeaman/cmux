import CmuxNextWakeups
import CmuxNextWakeupsTestSupport
import Foundation
import Testing
@testable import CmuxNextDaemon

/// The shared agent carrier while its connection reconnects (p01-process-236): an agent pane that reattaches
/// between two transports waits for the next admitted one, as the tree does, instead of finding no carrier and
/// opening a connection of its own.
///
/// The connection's clock is a ManualClock the test never advances: only the admitted connection can end the wait.
struct AgentSessionCarrierReconnectTests {
    /// A daemon whose first process exits on the first `ping`; the next process, the reconnect, stays.
    private static let script = #"""
    import json, os, sys
    marker = sys.argv[1]
    print('{"ok":true}', file=sys.stderr, flush=True)
    for line in sys.stdin:
        frame = json.loads(line)
        data = {}
        command = frame.get('cmd')
        if command == 'identify':
            data = {'app': 'cmux-tui', 'protocol': 12, 'generation': 'agent-carrier-test', 'capabilities': ['agent-session-attach-v1']}
        elif command == 'ping' and not os.path.exists(marker):
            open(marker, 'w').close()
            os._exit(0)
        print(json.dumps({'id': frame.get('id'), 'ok': True, 'data': data}), flush=True)
    """#

    @Test func aCarrierAskedWhileTheConnectionReconnectsIsTheNextConnection() async throws {
        let clock = ManualClock()
        let marker = NSTemporaryDirectory() + "cmux-agent-carrier-\(UUID().uuidString)"
        defer { try? FileManager.default.removeItem(atPath: marker) }
        let (parks, parked) = AsyncStream<Void>.makeStream()
        let wake = RetryWake(owner: "AgentSessionCarrierReconnectTests", probe: RetryWakeProbe(parked: { _ in parked.yield() }))
        let bridge = DaemonBridge(executable: "/usr/bin/python3", arguments: ["-u", "-c", Self.script, marker])
        // timedRetries 0: the reconnect waits for `wake` alone, never for a timer on the clock.
        let connection = DaemonConnection(
            endpoint: .init(socketPath: "/tmp/cmux-agent-carrier-test-\(UUID().uuidString).sock", bridge: bridge),
            configuration: .init(requiredCapabilities: [], retry: RetryPolicy(initial: .milliseconds(50), maximum: .seconds(1), timedRetries: 0),
                                 retryWake: wake, requestTimeout: .seconds(1), terminalEnvironment: nil),
            clock: clock)
        try await connection.start()
        let first = try #require(connection.agentSessionCarrier())

        // The daemon exits on this ping: the connection parks its reconnect until the wake.
        _ = try? await first.transport.request(cmd: "ping", timeout: .seconds(5)) { Data(#"{"id":\#($0),"cmd":"ping"}"#.utf8) }
        var waits = parks.makeAsyncIterator()
        _ = await waits.next()
        #expect(connection.agentSessionCarrier() == nil, "a closed transport carried an agent pane")

        let asked = Task { try await connection.agentSessionCarrier(within: .seconds(30)) }
        wake.fire()
        let next = try await Deadline(.seconds(5)).run("AgentSessionCarrierReconnectTests") { try await asked.value }
        await connection.close()

        #expect(next.transport !== first.transport, "the carrier is the reconnected transport")
        #expect(!next.ownsConnection)
    }

    @Test func aClosedConnectionHasNoCarrierAtOnce() async throws {
        let connection = DaemonConnection(endpoint: .init(socketPath: "/tmp/cmux-agent-carrier-closed-\(UUID().uuidString).sock"),
                                          clock: ManualClock())
        await connection.close()
        await #expect(throws: DaemonError.notConnected) { try await connection.agentSessionCarrier(within: .seconds(30)) }
    }
}
