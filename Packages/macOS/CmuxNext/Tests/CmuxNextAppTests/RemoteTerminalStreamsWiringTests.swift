@testable import CmuxNextApp
@testable import CmuxNextDaemon
@testable import CmuxNextDaemonService
import CmuxNextFeatures
import CmuxNextRemote
import CmuxNextWakeups
import Foundation
import Testing

/// The production wiring of `terminalStreams` for remote machines (p05-ipc-151): on, the registry hands this
/// Mac's sizing device to every SSH and server machine it adds, and the configuration `start(remote:)` connects
/// with streams their terminal views on the one bridged connection, echoes the grant and joins shared sizing;
/// off (the default), a remote machine's connection asks for none and each view keeps a connection of its own.
@MainActor
struct RemoteTerminalStreamsWiringTests {
    enum Kind: CaseIterable, Sendable { case ssh, server }

    private static func features(enabling names: Set<String>) -> FeatureList {
        FeatureList(registry: FeatureRegistry(), channel: .dev, overrides: FeatureOverrides(environmentEnabled: names))
    }

    /// A machine of `kind` added to a registry built as `AppServices` builds it, from `features`.
    private static func addedMachine(_ kind: Kind, features: FeatureList) throws -> DaemonService {
        let machines = MachineRegistry(local: DaemonService(), terminalStreams: CmuxNextApp.terminalStreams(features))
        let root = URL(fileURLWithPath: "/private/tmp/cn-remote-terminal-streams-unused")
        let mainWork = MainWorkScheduler(budget: MainFrameBudget())
        switch kind {
        case .ssh:
            let host = try SSHHost(destination: SSHDestination(parsing: "leo@huge-blue.tail1.ts.net")).named("lab")
            let session = SSHMachineSession(host: host, binary: URL(fileURLWithPath: "/usr/bin/false"), paths: SSHPaths(root: root),
                                            environment: { [:] }, mainWork: mainWork)
            machines.add(session)
            return session.daemon
        case .server:
            let reach = try ServerReach(hostID: "host_streams01", installID: "inst_streams01", name: "Server",
                                        route: .unix("/tmp/cmux-remote-terminal-streams.sock"))
            let session = ServerMachineSession(reach: reach, binary: nil, paths: .init(root: root), environment: { [:] },
                                               localIdentity: { nil }, mainWork: mainWork)
            machines.add(session)
            return session.daemon
        }
    }

    @Test(arguments: Kind.allCases)
    func onARemoteMachinesConnectionStreamsTerminalsAsThisMac(_ kind: Kind) async throws {
        let daemon = try Self.addedMachine(kind, features: Self.features(enabling: ["terminalStreams"]))
        #expect(daemon.terminalStreams != nil, "the registry hands the feature's device to the machine it adds")
        let configuration = daemon.remoteConfiguration
        #expect(configuration.streamsTerminals)
        #expect(configuration.handshakeCapabilities.contains(TerminalAttachment.streamsCapability))
        #expect(configuration.handshakeCapabilities.contains(DaemonCapability.sharedSizing.rawValue))
        let device = try #require(configuration.streamSizingDevice)
        #expect(await device().kind == "mac")
    }

    @Test(arguments: Kind.allCases)
    func offARemoteMachinesConnectionKeepsAConnectionPerView(_ kind: Kind) throws {
        let daemon = try Self.addedMachine(kind, features: Self.features(enabling: []))
        let handed = daemon.terminalStreams != nil
        #expect(!handed)
        let configuration = daemon.remoteConfiguration
        #expect(!configuration.streamsTerminals)
        let hasDevice = configuration.streamSizingDevice != nil
        #expect(!hasDevice)
        #expect(!configuration.handshakeCapabilities.contains(TerminalAttachment.streamsCapability))
        #expect(!configuration.handshakeCapabilities.contains(DaemonCapability.sharedSizing.rawValue))
    }
}
