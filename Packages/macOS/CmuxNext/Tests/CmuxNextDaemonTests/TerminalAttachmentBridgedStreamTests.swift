import Foundation
import Testing
@testable import CmuxNextDaemon

extension TerminalAttachmentStreamTests {
    /// A remote machine's terminal views on its one bridged connection (p05-ipc-151): the connection
    /// `DaemonService.start(remote:bridge:)` makes (``DaemonConnectionConfiguration/remote(retryWake:echoesPlacementShift:hangWatch:terminalStreams:)``)
    /// streams its views, so a view spawns no `cmux link dial` child and sends no handshake over the link; a daemon
    /// that does not offer `attach-streams-v1` keeps a bridged connection per view. The fake bridge is a child that
    /// pipes its stdin and stdout to ``StreamWireDaemon``'s socket after the dial line, as `cmux link dial` does.
    @Suite(.timeLimit(.minutes(1)))
    struct OnABridge {
        private static let size = CellSize(cols: 80, rows: 24)
        private static let device = TerminalSizingDevice(kind: "mac", name: "bridge test")
        private static let views: [UInt64] = [1, 2, 3, 4, 5]

        /// `cmux link dial`'s contract: one JSON dial line on stderr, then the daemon's stream on stdin and stdout.
        private static let bridgeScript = #"""
        import os, socket, sys, threading
        daemon = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        daemon.connect(sys.argv[1])
        os.write(2, b'{"ok":true}\n')
        def up():
            while True:
                data = os.read(0, 65536)
                if not data:
                    break
                daemon.sendall(data)
            daemon.shutdown(socket.SHUT_WR)
        threading.Thread(target=up, daemon=True).start()
        while True:
            data = daemon.recv(65536)
            if not data:
                break
            view = memoryview(data)
            while view:
                view = view[os.write(1, view):]
        """#

        private static func configuration(streams: Bool) -> DaemonConnection.Configuration {
            let device = Self.device
            var configuration = DaemonConnection.Configuration.remote(
                retryWake: RetryWake(owner: "TerminalAttachmentStreamTests.OnABridge"), echoesPlacementShift: false,
                hangWatch: nil, terminalStreams: streams ? { device } : nil)
            configuration.requiredCapabilities = []
            configuration.heartbeat = .seconds(300)
            configuration.hiddenHeartbeat = nil
            return configuration
        }

        private static func connect(_ daemon: StreamWireDaemon, stats: DaemonWireStats,
                                    streams: Bool = true) async throws -> DaemonConnection {
            let bridge = DaemonBridge(executable: "/usr/bin/python3", arguments: ["-u", "-c", bridgeScript, daemon.path])
            let connection = DaemonConnection(endpoint: DaemonEndpoint(socketPath: daemon.path, bridge: bridge, wireStats: stats),
                                              configuration: configuration(streams: streams))
            _ = try await connection.start()
            return connection
        }

        private static func attachAll(on connection: DaemonConnection) async throws -> [TerminalAttachment] {
            var attached: [TerminalAttachment] = []
            for surface in views {
                attached.append(try await TerminalAttachment.attach(
                    on: connection, target: .init(surface: SurfaceID(rawValue: surface)), size: size, claimGeometry: false))
            }
            return attached
        }

        private static func firstOutput(of attachment: TerminalAttachment) async -> Data? {
            for await event in attachment.events {
                if case .output(let data, _) = event { return data }
            }
            return nil
        }

        /// Red without the remote configuration's streams (each view spawned its own bridge child and sent its own
        /// identify and set-client-info over the link): five views of five terminals ride the one bridged control
        /// connection, each with one request, and each gets its own terminal's output.
        @Test func fiveRemoteViewsSpawnNoMoreBridgeChildrenAndNoHandshake() async throws {
            let stats = DaemonWireStats()
            let daemon = try StreamWireDaemon(.init(outputBytes: Dictionary(uniqueKeysWithValues: Self.views.map { ($0, 100) })))
            defer { daemon.stop() }
            let connection = try await Self.connect(daemon, stats: stats)
            #expect(stats.snapshot().bridgeSpawnsTotal == 1, "the control connection's own child")

            let attached = try await Self.attachAll(on: connection)
            for (surface, view) in zip(Self.views, attached) {
                let byte = UInt8(ascii: "a") + UInt8(surface % 26)
                #expect(await Self.firstOutput(of: view)?.allSatisfy { $0 == byte } == true)
            }
            let snapshot = stats.snapshot()
            #expect(snapshot.bridgeSpawnsTotal == 1, "no child per view")
            #expect(snapshot.connectionsOpened[.terminalAttach, default: 0] == 0)
            #expect(daemon.connections == 1)
            // One round trip on the same connection orders every view's request ahead of its reply.
            _ = try await connection.request(IdentifyRequest(), timeout: .seconds(5))
            let all = daemon.requests
            let handshake = try #require(all.firstIndex { $0.cmd == SubscribeRequest.command })
            let views = Array(all[(handshake + 1)...].dropLast())
            #expect(views.map(\.cmd) == Array(repeating: AttachSurfaceRequest.command, count: Self.views.count))
            #expect(views.map(\.stream) == [1, 2, 3, 4, 5])
            for view in attached { view.detachNow() }
            await connection.close()
        }

        /// A bridged daemon without `attach-streams-v1` (an older build on the machine) keeps today's path: each
        /// view opens a connection of its own through its own bridge child.
        @Test func anOldDaemonKeepsABridgedConnectionPerView() async throws {
            let stats = DaemonWireStats()
            let daemon = try StreamWireDaemon(.init(offersStreams: false,
                                                    outputBytes: Dictionary(uniqueKeysWithValues: Self.views.map { ($0, 10) })))
            defer { daemon.stop() }
            let connection = try await Self.connect(daemon, stats: stats)
            let attached = try await Self.attachAll(on: connection)
            #expect(attached.allSatisfy { $0.stream == nil })
            for view in attached { #expect(await Self.firstOutput(of: view) != nil) }
            let snapshot = stats.snapshot()
            #expect(snapshot.bridgeSpawnsTotal == UInt64(1 + Self.views.count))
            #expect(snapshot.bridgeSpawns[.terminalAttach, default: 0] == UInt64(Self.views.count))
            #expect(daemon.requests.filter { $0.cmd == AttachSurfaceRequest.command }.allSatisfy { $0.stream == nil })
            for view in attached { view.detachNow() }
            await connection.close()
        }

        /// The remote configuration asks for streams and joins shared sizing as this Mac only with a device (the
        /// `terminalStreams` feature on); without one it asks for neither.
        @Test func theRemoteConfigurationStreamsTerminalsOnlyWithADevice() async throws {
            let on = Self.configuration(streams: true)
            #expect(on.streamsTerminals)
            #expect(on.sessionEvents)
            #expect(on.handshakeCapabilities.contains(TerminalAttachment.streamsCapability))
            #expect(on.handshakeCapabilities.contains(DaemonCapability.sharedSizing.rawValue))
            let device = try #require(on.streamSizingDevice)
            #expect(await device() == Self.device)

            let off = Self.configuration(streams: false)
            #expect(!off.streamsTerminals)
            #expect(off.sessionEvents)
            #expect(!off.handshakeCapabilities.contains(TerminalAttachment.streamsCapability))
        }
    }
}
