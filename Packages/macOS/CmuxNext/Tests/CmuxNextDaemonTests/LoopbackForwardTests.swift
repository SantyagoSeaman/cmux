import CmuxNextCompat
@testable import CmuxNextDaemon
import Darwin
import Foundation
import Testing

/// `loopback-forward-v1` streams on a shared daemon connection (p05-ipc-101): the connection's table mints the
/// ids, its reader hands the stream events to the table and never to the control sequence, the stream lines go
/// out on the connection, and the connection's end ends every stream after the data routed before it.
@Suite(.serialized, .timeLimit(.minutes(1))) struct LoopbackForwardTests {
    /// One accepted connection of a fake daemon on a Unix socket in `/tmp` that reads whole lines.
    final class FakeDaemon {
        let path = "/tmp/cmux-lbf-\(UUID().uuidString.prefix(8)).sock"
        private var listener: Int32 = -1
        private var peer: Int32 = -1

        init() throws {
            listener = socket(AF_UNIX, SOCK_STREAM, 0)
            var address = sockaddr_un()
            address.sun_family = sa_family_t(AF_UNIX)
            let path = path
            withUnsafeMutableBytes(of: &address.sun_path) { buffer in
                buffer.copyBytes(from: path.utf8)
                buffer[path.utf8.count] = 0
            }
            let bound = withUnsafePointer(to: &address) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    bind(listener, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
                }
            }
            guard bound == 0, listen(listener, 1) == 0 else { throw POSIXError(.EADDRINUSE) }
        }

        deinit {
            hangUp()
            if listener >= 0 { close(listener) }
            unlink(path)
        }

        func accept() throws {
            peer = Darwin.accept(listener, nil, nil)
            guard peer >= 0 else { throw POSIXError(.ECONNABORTED) }
        }

        /// Reads one line the transport wrote, as a JSON object.
        func readObject() throws -> [String: Any] {
            var line = Data()
            var byte: UInt8 = 0
            while true {
                guard read(peer, &byte, 1) == 1 else { throw POSIXError(.ECONNRESET) }
                if byte == 0x0A { break }
                line.append(byte)
            }
            return try #require(try JSONSerialization.jsonObject(with: line) as? [String: Any])
        }

        func write(_ lines: [String]) {
            var bytes = Array(lines.joined(separator: "\n").utf8)
            bytes.append(0x0A)
            var offset = 0
            while offset < bytes.count {
                let written = bytes[offset...].withUnsafeBytes { Darwin.write(peer, $0.baseAddress, $0.count) }
                guard written > 0 else { return }
                offset += written
            }
        }

        /// Closes the daemon's end: the transport reads EOF.
        func hangUp() {
            if peer >= 0 { close(peer) }
            peer = -1
        }
    }

    /// The events the control sequence got (`onEvent`).
    final class ControlEvents: Sendable {
        let names = Mutex<[String]>([])
        let arrivals: AsyncStream<Void>
        private let continuation: AsyncStream<Void>.Continuation

        init() {
            let (arrivals, continuation) = AsyncStream<Void>.makeStream()
            self.arrivals = arrivals
            self.continuation = continuation
        }

        func record(_ name: String) {
            names.withLock { $0.append(name) }
            continuation.yield()
        }

        func waitForOne() async {
            for await _ in arrivals { return }
        }
    }

    /// Stands for a ``LoopbackForwardClient``: a table entry's owner.
    final class Owner {}

    private func connect(_ daemon: FakeDaemon, events: ControlEvents) throws -> LineTransport {
        let transport = try LineTransport(path: daemon.path, role: .control, stats: nil)
        try daemon.accept()
        transport.start(onEvent: { name, _, _ in events.record(name) }, onClose: { _ in })
        return transport
    }

    private func openStream(on transport: LineTransport, owner: Owner, window: Int = 1024) throws -> LoopbackStream {
        let table = try #require(transport.loopbackStreams())
        return try #require(table.makeStream(owner: ObjectIdentifier(owner), receiveWindow: window,
                                             sender: TransportLineSender(transport: transport)))
    }

    @Test func streamEventsReachTheirStreamAndNeverTheControlSequence() async throws {
        let daemon = try FakeDaemon()
        let events = ControlEvents()
        let transport = try connect(daemon, events: events)
        defer { transport.close() }
        let owner = Owner()
        let stream = try openStream(on: transport, owner: owner)

        daemon.write([
            #"{"event":"loopback-data","stream":\#(stream.id),"data":"aGk="}"#,
            #"{"event":"loopback-eof","stream":\#(stream.id)}"#,
            #"{"event":"sentinel"}"#,
        ])
        var received = stream.events.makeAsyncIterator()
        #expect(await received.next() == .data(Data("hi".utf8)))
        #expect(await received.next() == .eof)
        await events.waitForOne()

        #expect(events.names.withLock { $0 } == ["sentinel"], "a loopback event reached the control sequence")
        #expect(transport.routedEventCount == 1, "a loopback event moved the control sequence's barrier")
    }

    @Test func clientsSharingAConnectionGetDistinctIDsAndCloseOnlyTheirOwn() throws {
        let daemon = try FakeDaemon()
        let transport = try connect(daemon, events: ControlEvents())
        defer { transport.close() }
        let first = Owner(), second = Owner()
        let a = try openStream(on: transport, owner: first)
        let b = try openStream(on: transport, owner: second)
        let c = try openStream(on: transport, owner: first)
        #expect(Set([a.id, b.id, c.id]).count == 3, "two clients on one connection share a stream id")

        let table = try #require(transport.loopbackStreams())
        #expect(table.count(owner: ObjectIdentifier(first)) == 2)
        let closed = table.removeAll(owner: ObjectIdentifier(first))
        #expect(Set(closed.map(\.id)) == [a.id, c.id])
        #expect(table.stream(b.id) === b, "closing one client's streams took another's")
        #expect(table.count(owner: ObjectIdentifier(second)) == 1)
    }

    @Test func theConnectionsEndFailsEveryStreamAfterItsData() async throws {
        let daemon = try FakeDaemon()
        let transport = try connect(daemon, events: ControlEvents())
        defer { transport.close() }
        let stream = try openStream(on: transport, owner: Owner())

        daemon.write([#"{"event":"loopback-data","stream":\#(stream.id),"data":"aGk="}"#])
        daemon.hangUp()
        var received = stream.events.makeAsyncIterator()
        #expect(await received.next() == .data(Data("hi".utf8)), "the data routed before the end was lost")
        #expect(await received.next() == .closed(error: .connectionLost("EOF")))
        #expect(await received.next() == nil)
    }

    @Test func noStreamJoinsAClosedConnection() throws {
        let daemon = try FakeDaemon()
        let transport = try connect(daemon, events: ControlEvents())
        let table = try #require(transport.loopbackStreams())
        transport.close()

        #expect(transport.loopbackStreams() == nil, "a closed connection made a table")
        let late = table.makeStream(owner: ObjectIdentifier(Owner()), receiveWindow: 1024,
                                    sender: TransportLineSender(transport: transport))
        #expect(late == nil, "a stream joined a table whose connection ended")
    }

    @Test func aStreamsDataGoesOutOnItsConnection() async throws {
        let daemon = try FakeDaemon()
        let transport = try connect(daemon, events: ControlEvents())
        defer { transport.close() }
        let stream = try openStream(on: transport, owner: Owner())
        stream.opened(address: "127.0.0.1:5173", sendWindow: 1024)

        try await stream.write(Data("hello".utf8))
        stream.shutdownWrite()

        let data = try daemon.readObject()
        #expect(data["cmd"] as? String == "loopback-data")
        #expect((data["stream"] as? NSNumber)?.uint64Value == stream.id)
        #expect(data["data"] as? String == Data("hello".utf8).base64EncodedString())
        let shutdown = try daemon.readObject()
        #expect(shutdown["cmd"] as? String == "loopback-shutdown", "the half close passed the data it follows")
    }
}
