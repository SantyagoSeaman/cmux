@testable import CmuxNextDaemon
import Foundation
import Testing

/// ``LoopbackForwardClient`` rides the machine's control connection (p05-ipc-101): its streams go out on that
/// connection's transport with ids its table mints, a daemon or connection without `loopback-forward-v1` refuses
/// it, the next open after a reconnect rides the new epoch, and its `close()` ends only its own streams and never
/// the shared connection.
@Suite(.serialized, .timeLimit(.minutes(1))) struct LoopbackForwardClientTests {
    /// A daemon on the bridge's stdio. It answers `identify` with `argv[2]`'s capabilities, `loopback-open` with
    /// an address that names its process (`gen1` for the first one, `gen2` for a reconnect) and the stream id it
    /// read, every other request with `{}`. A `loopback-data` line makes it exit: the connection drops.
    private static let script = #"""
    import json, os, sys
    marker = sys.argv[1]
    caps = [c for c in sys.argv[2].split(',') if c]
    generation = 'gen2' if os.path.exists(marker) else 'gen1'
    open(marker, 'a').close()
    print('{"ok":true}', file=sys.stderr, flush=True)
    for line in sys.stdin:
        frame = json.loads(line)
        command = frame.get('cmd')
        if 'id' not in frame:
            if command == 'loopback-data':
                os._exit(0)
            continue
        data = {}
        if command == 'identify':
            data = {'app': 'cmux-tui', 'protocol': 12, 'generation': 'loopback-client-test', 'capabilities': caps}
        elif command == 'loopback-open':
            data = {'stream': frame['stream'], 'address': '%s:%d' % (generation, frame['stream']), 'window': 65536}
        print(json.dumps({'id': frame['id'], 'ok': True, 'data': data}), flush=True)
    """#

    private let marker = NSTemporaryDirectory() + "cmux-loopback-client-\(UUID().uuidString)"

    /// A started connection to the fake daemon. `advertised` nil: the default list, which has `loopback-forward-v1`.
    private func connection(daemonCapabilities: [String] = [DaemonCapability.loopbackForward.rawValue],
                            advertised: [String]? = nil) async throws -> DaemonConnection {
        let bridge = DaemonBridge(executable: "/usr/bin/python3",
                                  arguments: ["-u", "-c", Self.script, marker, daemonCapabilities.joined(separator: ",")])
        var configuration = DaemonConnectionConfiguration(
            requiredCapabilities: [],
            retry: RetryPolicy(initial: .milliseconds(20), maximum: .milliseconds(200), timedRetries: 10),
            requestTimeout: .seconds(2), terminalEnvironment: nil)
        if let advertised { configuration.advertisedCapabilities = advertised }
        let connection = DaemonConnection(
            endpoint: .init(socketPath: "/tmp/cmux-loopback-client-\(UUID().uuidString.prefix(8)).sock", bridge: bridge),
            configuration: configuration)
        try await connection.start()
        return connection
    }

    private func isConnectionLoss(_ event: LoopbackStreamEvent?) -> Bool {
        switch event {
        case .closed(error: .connectionLost): true
        default: false
        }
    }

    @Test func openSendsLoopbackOpenOnTheConnectionsTransportWithATableID() async throws {
        defer { try? FileManager.default.removeItem(atPath: marker) }
        let connection = try await connection()
        let first = LoopbackForwardClient(connection: { connection })
        let second = LoopbackForwardClient(connection: { connection })
        let a = try await first.open(host: "localhost", port: 5173)
        let b = try await second.open(host: "127.0.0.1", port: 8080)
        let ready = try #require(await connection.ready)

        #expect(a.id != b.id, "two clients on one connection got the same stream id")
        #expect(a.address == "gen1:\(a.id)", "the daemon did not read loopback-open with the stream's id")
        #expect(b.address == "gen1:\(b.id)", "the daemon did not read loopback-open with the stream's id")
        #expect(try await first.liveTransport() === ready.transport, "the client opened a connection of its own")
        #expect(try await second.liveTransport() === ready.transport, "the client opened a connection of its own")
        let table = try #require(ready.transport.loopbackStreams())
        #expect(table.stream(a.id) === a && table.stream(b.id) === b, "the stream ids are not the connection table's")

        await first.close()
        await second.close()
        await connection.close()
    }

    @Test func aConnectionWithoutLoopbackForwardRefusesOpen() async throws {
        defer { try? FileManager.default.removeItem(atPath: marker) }
        // The daemon lacks the capability; then the daemon has it and the connection did not echo it.
        let connections = [
            try await connection(daemonCapabilities: []),
            try await connection(advertised: DaemonCapability.advertised.filter { $0 != LoopbackForwardClient.capability }),
        ]
        for connection in connections {
            let client = LoopbackForwardClient(connection: { connection })
            await #expect(throws: LoopbackForwardError.unsupported) {
                _ = try await client.open(host: "localhost", port: 5173)
            }
            await client.close()
            await connection.close()
        }
    }

    @Test func theOpenAfterAReconnectRidesTheNewEpoch() async throws {
        defer { try? FileManager.default.removeItem(atPath: marker) }
        let connection = try await connection()
        let client = LoopbackForwardClient(connection: { connection })
        let before = try await client.open(host: "localhost", port: 5173)
        let first = try #require(await connection.ready)
        #expect(before.address == "gen1:\(before.id)")

        // The daemon exits on the stream's data: the connection drops and reconnects to the next process.
        var events = before.events.makeAsyncIterator()
        try await before.write(Data("bye".utf8))
        #expect(isConnectionLoss(await events.next()), "the stream outlived its connection")
        #expect(first.transport.isClosed)

        let after = try await client.open(host: "localhost", port: 5173)
        let second = try #require(await connection.ready)
        #expect(second.serial > first.serial)
        #expect(after.address == "gen2:\(after.id)", "the open did not reach the reconnected daemon")
        #expect(try await client.liveTransport() === second.transport, "the client rides an old epoch's transport")
        #expect(second.transport.loopbackStreams()?.stream(after.id) === after)

        await client.close()
        await connection.close()
    }

    @Test func closeEndsOnlyThisClientsStreamsAndKeepsTheConnection() async throws {
        defer { try? FileManager.default.removeItem(atPath: marker) }
        let connection = try await connection()
        let closing = LoopbackForwardClient(connection: { connection })
        let staying = LoopbackForwardClient(connection: { connection })
        let closed = try await closing.open(host: "localhost", port: 5173)
        let kept = try await staying.open(host: "localhost", port: 3000)
        let ready = try #require(await connection.ready)

        await closing.close()

        var events = closed.events.makeAsyncIterator()
        #expect(await events.next() == .closed(error: .closedLocally))
        #expect(!ready.transport.isClosed, "closing a client closed the machine's control connection")
        #expect(await connection.ready?.serial == ready.serial, "closing a client dropped the connection")
        let table = try #require(ready.transport.loopbackStreams())
        #expect(table.count(owner: ObjectIdentifier(closing)) == 0)
        #expect(table.stream(kept.id) === kept, "closing one client ended another's stream")
        let another = try await staying.open(host: "localhost", port: 3001)
        #expect(another.address == "gen1:\(another.id)", "the shared connection stopped answering")
        await #expect(throws: LoopbackForwardError.self) { _ = try await closing.open(host: "localhost", port: 5173) }

        await staying.close()
        await connection.close()
    }
}
