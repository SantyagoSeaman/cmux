import CmuxNextCompat
import Foundation
import Testing
@testable import CmuxNextDaemon

/// An agent chat attachment is a stream on the machine's one connection (p05-ipc-102): no socket of
/// its own, its pushes reach the client of their surface on the reader and never the connection's
/// event consumers, the last client of a tab detaches it, and a lost connection closes every
/// attachment it carried. The daemon is a scripted fixture behind a ``DaemonBridge``: one bridge
/// process per connection, so the fixture counts connections by its `identify` lines.
@Suite(.timeLimit(.minutes(1))) struct AgentSessionAttachTests {
    private struct Calls: DaemonRequest {
        typealias Response = JSONValue
        static let commandName: DaemonCommandName = "test-calls"
    }

    private struct Exit: DaemonRequest {
        typealias Response = JSONValue
        static let commandName: DaemonCommandName = "test-exit"
    }

    /// Answers the handshake and every verb; an attach is answered with a page, then one record for
    /// the attached surface, one for surface 99 and one `agent-session-closed` for surface 99.
    private static let script = #"""
    import json, sys
    calls = []
    print('{"ok":true}', file=sys.stderr, flush=True)
    def send(obj):
        print(json.dumps(obj), flush=True)
    for line in sys.stdin:
        frame = json.loads(line)
        cmd = frame.get('cmd')
        if cmd == 'test-exit':
            sys.exit(0)
        if cmd != 'test-calls':
            calls.append({'cmd': cmd, 'surface': frame.get('surface')})
        if cmd == 'identify':
            data = {'app':'cmux-tui','protocol':12,'generation':'asa','capabilities':['agent-session-attach-v1']}
        elif cmd == 'test-calls':
            data = calls
        elif cmd == 'agent-session-attach':
            data = {'session':{'sessionId':'s1'},'records':[]}
        else:
            data = {}
        send({'id':frame.get('id'),'ok':True,'data':data})
        if cmd == 'agent-session-attach':
            surface = frame['surface']
            send({'event':'agent-session-record','surface':surface,'record':{'seq':1}})
            send({'event':'agent-session-record','surface':99,'record':{'seq':1}})
            send({'event':'agent-session-closed','surface':99,'reason':'detached'})
    """#

    private static func connection() -> DaemonConnection {
        let bridge = DaemonBridge(executable: "/usr/bin/python3", arguments: ["-u", "-c", script])
        let endpoint = DaemonEndpoint(socketPath: "/tmp/cmux-asa-test-\(UUID().uuidString).sock", bridge: bridge)
        return DaemonConnection(endpoint: endpoint, configuration: .init(requiredCapabilities: [], terminalEnvironment: nil))
    }

    /// The pushes one client got, as `(event, surface or reason)`.
    final class Pushes: Sendable {
        let stream: AsyncStream<(String, String)>
        private let continuation: AsyncStream<(String, String)>.Continuation

        init() {
            (stream, continuation) = AsyncStream.makeStream(of: (String, String).self)
        }

        var handler: AgentSessionAttachClient.EventHandler {
            { [continuation] name, line in
                let object = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] ?? [:]
                let tag = (object["surface"] as? NSNumber).map { "\($0.uint64Value)" } ?? (object["reason"] as? String ?? "")
                continuation.yield((name, tag))
            }
        }

        func next(_ count: Int) async -> [String] {
            var got: [String] = []
            for await (name, tag) in stream {
                got.append("\(name) \(tag)")
                if got.count == count { break }
            }
            return got
        }
    }

    private static func calls(_ connection: DaemonConnection) async throws -> [String] {
        let calls = try #require(try await connection.request(Calls()).arrayValue)
        return calls.map { call in
            let surface = call["surface"]?.doubleValue.map { " \(UInt64($0))" } ?? ""
            return (call["cmd"]?.stringValue ?? "") + surface
        }
    }

    @Test func anAttachRidesTheMachineConnectionAndGetsOnlyItsSurfacesPushes() async throws {
        let connection = Self.connection()
        try await connection.start()
        let observed = connection.subscribe(filter: DaemonEventFilter { event in
            if case .unknown(let name, _) = event { name.hasPrefix("agent-session-") } else { false }
        })
        let pushes = Pushes()
        let client = AgentSessionAttachClient(surface: 7) { connection }
        let page = try await client.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: pushes.handler)
        #expect(String(decoding: page, as: UTF8.self).contains("s1"))
        // Surface 99's record and close never reach surface 7's client.
        #expect(await pushes.next(1) == ["agent-session-record 7"])
        await client.close()
        let calls = try await Self.calls(connection)
        // One connection (one identify), and the close detached the tab on it.
        #expect(calls.filter { $0 == "identify" }.count == 1)
        #expect(calls.filter { $0.hasPrefix("agent-session-") } == ["agent-session-attach 7", "agent-session-detach 7"])
        #expect(connection.agentSessions.count == 0)
        await connection.close()
        var leaked: [DaemonSubscriptionItem] = []
        for await item in observed { leaked.append(item) }
        #expect(leaked.isEmpty, "agent session pushes reached the connection's event consumers")
    }

    @Test func theLastClientOfATabDetachesIt() async throws {
        let connection = Self.connection()
        try await connection.start()
        let first = AgentSessionAttachClient(surface: 7) { connection }
        let second = AgentSessionAttachClient(surface: 7) { connection }
        let (one, two) = (Pushes(), Pushes())
        _ = try await first.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: one.handler)
        _ = try await second.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: two.handler)
        await first.close()
        #expect(try await Self.calls(connection).filter { $0.hasPrefix("agent-session-detach") }.isEmpty)
        await second.close()
        #expect(try await Self.calls(connection).filter { $0.hasPrefix("agent-session-detach") } == ["agent-session-detach 7"])
        await connection.close()
    }

    @Test func aLostConnectionClosesEveryAttachmentOnce() async throws {
        let connection = Self.connection()
        try await connection.start()
        let pushes = Pushes()
        let client = AgentSessionAttachClient(surface: 7) { connection }
        _ = try await client.attach(afterSeq: nil, beforeSeq: nil, limit: nil, kinds: nil, events: pushes.handler)
        _ = try? await connection.request(Exit(), timeout: .seconds(5))
        #expect(await pushes.next(2) == ["agent-session-record 7", "agent-session-closed connection_lost"])
        #expect(connection.agentSessions.count == 0)
        await client.close()
        await connection.close()
    }

    @Test func theRouteHandsAPushToItsSurfaceOnItsSerialAndCountsViewers() {
        let route = AgentSessionStreamRoute()
        let decoder = WireDecoder(recorder: nil)
        let got = Got()
        let a = route.open(surface: 7, serial: 1) { name, _ in got.add("a \(name)") }
        let b = route.open(surface: 7, serial: 1) { name, _ in got.add("b \(name)") }
        let old = route.open(surface: 7, serial: 0) { name, _ in got.add("old \(name)") }
        route.deliver(name: "agent-session-record", line: Data(#"{"event":"agent-session-record","surface":7}"#.utf8),
                      serial: 1, decoder: decoder)
        route.deliver(name: "agent-session-record", line: Data(#"{"event":"agent-session-record","surface":8}"#.utf8),
                      serial: 1, decoder: decoder)
        #expect(got.all == ["a agent-session-record", "b agent-session-record"])
        #expect(!route.close(a))
        #expect(route.close(b))
        #expect(route.close(old))
        #expect(!route.close(b))
        _ = route.open(surface: 7, serial: 2) { name, line in got.add("lost \(name) \(String(decoding: line, as: UTF8.self))") }
        route.end(serial: 2)
        route.end(serial: 2)
        #expect(got.all.last == "lost agent-session-closed " + String(decoding: AgentSessionStreamRoute.connectionLost, as: UTF8.self))
        #expect(got.all.count == 3)
        #expect(route.count == 0)
    }

    final class Got: Sendable {
        private let items = Mutex<[String]>([])
        func add(_ item: String) { items.withLock { $0.append(item) } }
        var all: [String] { items.withLock { $0 } }
    }
}
