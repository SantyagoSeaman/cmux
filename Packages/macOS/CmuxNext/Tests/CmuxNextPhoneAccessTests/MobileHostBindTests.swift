import CmuxIrxTransport
import CmuxNextDaemon
@testable import CmuxNextMobile
@testable import CmuxNextPhoneAccess
import CmuxNextWakeups
import CmuxNextWakeupsTestSupport
import Foundation
import Testing

/// Phone Access binds its iroh endpoint only when a phone can use it (p14-energy-122,
/// `phoneEndpointOnDemand`): no phone, no endpoint (no relay connection, no IrohLib thread);
/// the pairing page or a live attach ticket binds one; a paired phone keeps it.
@Suite(.timeLimit(.minutes(1))) struct MobileHostBindTests {
    // MARK: When the endpoint is wanted

    @Test func offOnDemandTheEndpointIsAlwaysWanted() {
        #expect(MobileIrxHost.wantsEndpoint(onDemand: false, pairedPhones: 0, pairingOpen: false, ticketHeld: false))
    }

    @Test func noPhoneNoEndpoint() {
        #expect(!MobileIrxHost.wantsEndpoint(onDemand: true, pairedPhones: 0, pairingOpen: false, ticketHeld: false))
    }

    @Test func thePairingPageBindsOne() {
        #expect(MobileIrxHost.wantsEndpoint(onDemand: true, pairedPhones: 0, pairingOpen: true, ticketHeld: false))
    }

    @Test func aLiveAttachTicketBindsOne() {
        #expect(MobileIrxHost.wantsEndpoint(onDemand: true, pairedPhones: 0, pairingOpen: false, ticketHeld: true))
    }

    /// The first pairing puts the phone in the account's directory, so closing the page keeps the endpoint.
    @Test func aPairedPhoneKeepsItAfterThePageCloses() {
        #expect(MobileIrxHost.wantsEndpoint(onDemand: true, pairedPhones: 1, pairingOpen: false, ticketHeld: false))
    }

    // MARK: Which peers are paired phones

    @Test func aPhoneInTheDirectoryIsPaired() throws {
        #expect(MobileIrxHost.pairedPhones(in: try Self.directory([("ios", false)])) == 1)
        #expect(MobileIrxHost.pairedPhones(in: try Self.directory([("ios", false), ("ios", false)])) == 2)
    }

    @Test func macsRevokedPhonesAndNoDirectoryAreNot() throws {
        #expect(MobileIrxHost.pairedPhones(in: nil) == 0)
        #expect(MobileIrxHost.pairedPhones(in: try Self.directory([])) == 0)
        #expect(MobileIrxHost.pairedPhones(in: try Self.directory([("mac", false)])) == 0)
        #expect(MobileIrxHost.pairedPhones(in: try Self.directory([("ios", true)])) == 0)
        #expect(MobileIrxHost.pairedPhones(in: try Self.directory([("mac", false), ("ios", true), ("ios", false)])) == 1)
    }

    // MARK: The listener

    @Test func aFreshListenerOnDemandWantsNoEndpoint() async throws {
        let host = try Self.host(onDemand: true)
        #expect(await !host.endpointWanted)
        let open = try Self.host(onDemand: true, pairingOpen: true)
        #expect(await open.endpointWanted)
        let always = try Self.host(onDemand: false)
        #expect(await always.endpointWanted)
    }

    @Test func aLatePairingHopIsDropped() async throws {
        let host = try Self.host(onDemand: true)
        await host.setPairingOpen(true, revision: 2)
        await host.setPairingOpen(false, revision: 1)
        #expect(await host.pairing.open)
        #expect(await host.endpointWanted)
        await host.setPairingOpen(false, revision: 3)
        #expect(await !host.endpointWanted)
    }

    @Test func mintingATicketHoldsTheEndpoint() async throws {
        let host = try Self.host(onDemand: true)
        await #expect(throws: MobileIrxHost.TicketFailure.self) {
            _ = try await host.attachTicket(ttl: 600, target: .physicalDevice, scheme: nil)
        }
        #expect(await host.ticketHold != nil)
        #expect(await host.endpointWanted)
        await host.stop()
        #expect(await host.ticketHold == nil)
    }

    @Test func offOnDemandATicketHoldsNothing() async throws {
        let host = try Self.host(onDemand: false)
        await #expect(throws: MobileIrxHost.TicketFailure.self) {
            _ = try await host.attachTicket(ttl: 600, target: .physicalDevice, scheme: nil)
        }
        #expect(await host.ticketHold == nil)
    }

    // MARK: A provisioned listener

    @Test func provisionedWithNoPhoneNothingBinds() async throws {
        let (host, endpoint) = try await Self.provisioned(onDemand: true)
        #expect(await host.phase == .standby)
        #expect(await endpoint.binds == 0)
        #expect(await host.endpointTask == nil)
    }

    @Test func offOnDemandTheProvisionedListenerBindsAtOnce() async throws {
        let (host, endpoint) = try await Self.provisioned(onDemand: false)
        await Self.phase(of: host, reaches: Self.listening)
        #expect(await endpoint.binds == 1)
    }

    @Test func thePairingPageBindsOneEndpointAndItsCloseReleasesIt() async throws {
        let (host, endpoint) = try await Self.provisioned(onDemand: true)
        await host.setPairingOpen(true, revision: 1)
        await Self.phase(of: host, reaches: Self.listening)
        let relayRead = await endpoint.watch(.relay)
        await host.setPairingOpen(true, revision: 2)
        for await _ in relayRead { break }
        #expect(await endpoint.binds == 1)
        #expect(await host.phase == Self.listening)
        await host.setPairingOpen(false, revision: 3)
        #expect(await host.phase == .standby)
        #expect(await endpoint.closes >= 1)
        #expect(await !endpoint.bound)
        #expect(await host.acceptTask == nil)
    }

    /// The first pairing puts the phone in the account's directory, so the endpoint stays when the page closes.
    @Test func aPairedPhoneKeepsTheEndpointWhenThePageCloses() async throws {
        let (host, endpoint) = try await Self.provisioned(onDemand: true, phones: 1)
        await Self.phase(of: host, reaches: Self.listening)
        await host.setPairingOpen(true, revision: 1)
        await host.setPairingOpen(false, revision: 2)
        #expect(await endpoint.closes == 0)
        #expect(await endpoint.binds == 1)
        #expect(await host.phase == Self.listening)
    }

    @Test func aTicketHoldsTheEndpointUntilItExpires() async throws {
        let clock = ManualClock()
        let (host, endpoint) = try await Self.provisioned(onDemand: true, clock: clock)
        await #expect(throws: MobileIrxHost.TicketFailure.self) {
            _ = try await host.attachTicket(ttl: 600, target: .physicalDevice, scheme: nil)
        }
        await Self.phase(of: host, reaches: Self.listening)
        try await clock.waitForSleeper()
        clock.advance(by: .seconds(600))
        await Self.phase(of: host, reaches: .standby)
        #expect(await endpoint.closes == 1)
        #expect(await host.ticketHold == nil)
    }

    /// The pairing page's 600 s QR ticket, then a 30 s `mobile.attach_ticket.create`: a phone that
    /// scans the QR code after 30 s still finds this Mac listening.
    @Test func aShorterTicketKeepsTheLongerTicketsHold() async throws {
        let clock = ManualClock()
        let (host, endpoint) = try await Self.provisioned(onDemand: true, clock: clock)
        _ = try? await host.attachTicket(ttl: 600, target: .physicalDevice, scheme: nil)
        await Self.phase(of: host, reaches: Self.listening)
        try await clock.waitForSleeper()
        let relayRead = await endpoint.watch(.relay)
        _ = try? await host.attachTicket(ttl: 30, target: .physicalDevice, scheme: nil)
        for await _ in relayRead { break }
        try await clock.waitForSleeper()
        clock.advance(by: .seconds(30))
        #expect(clock.sleeperCount == 1)
        #expect(await host.phase == Self.listening)
        #expect(await endpoint.closes == 0)
        clock.advance(by: .seconds(570))
        await Self.phase(of: host, reaches: .standby)
        #expect(await !endpoint.bound)
    }

    /// The page closes while a bind that returned reads its relay: the host stands by, starts no
    /// accept loop, and the endpoint that bind left is closed.
    @Test func aReleaseDuringABindThatReturnedStandsBy() async throws {
        let (host, endpoint) = try await Self.provisioned(onDemand: true)
        let bindReached = await endpoint.watch(.bind)
        let relayReached = await endpoint.watch(.relay)
        await endpoint.hold(.bind)
        await endpoint.hold(.relay)
        await host.setPairingOpen(true, revision: 1)
        let binding = try #require(await host.endpointTask)
        for await _ in bindReached { break }
        await endpoint.open(.bind)
        for await _ in relayReached { break }
        await host.setPairingOpen(false, revision: 2)
        #expect(await host.phase == .standby)
        await endpoint.open(.relay)
        await binding.value
        #expect(await host.phase == .standby)
        #expect(await host.acceptTask == nil)
        #expect(await endpoint.accepts == 0)
        #expect(await !endpoint.bound)
    }

    /// Closing the endpoint ends the accept loop's wait; that is not an accept failure, so nothing restarts.
    @Test func aReleasedEndpointRestartsNoAcceptLoop() async throws {
        let clock = ManualClock()
        let (host, endpoint) = try await Self.provisioned(onDemand: true, clock: clock)
        let accepting = await endpoint.watch(.accept)
        await host.setPairingOpen(true, revision: 1)
        for await _ in accepting { break }
        let loop = try #require(await host.acceptTask)
        await host.setPairingOpen(false, revision: 2)
        await loop.value
        #expect(await host.acceptRestart == nil)
        #expect(await host.acceptTask == nil)
        #expect(await host.phase == .standby)
        #expect(await endpoint.closes == 1)
    }

    // MARK: The service

    @Test func thePairingPageIsOpenUntilItsLastViewerCloses() {
        let service = MobileHostService(displayName: { "Mac" }, work: BackgroundWork(label: "test.mobile-bind"),
                                        endpointOnDemand: true)
        #expect(!service.pairingOpen)
        service.pairingPageOpened()
        service.pairingPageOpened()
        service.pairingPageClosed()
        #expect(service.pairingOpen)
        service.pairingPageClosed()
        #expect(!service.pairingOpen)
        service.pairingPageClosed()
        service.pairingPageOpened()
        #expect(service.pairingOpen)
    }

    /// The page's first open and last close reach the listener, with increasing revisions.
    @Test func thePairingPageReachesTheListener() async throws {
        let (host, endpoint) = try await Self.provisioned(onDemand: true)
        let service = MobileHostService(displayName: { "Mac" }, work: BackgroundWork(label: "test.mobile-bind"),
                                        endpointOnDemand: true)
        service.host = host
        service.pairingPageOpened()
        await Self.phase(of: host, reaches: Self.listening)
        #expect(await host.pairing.open)
        #expect(await host.pairing.revision == 1)
        service.pairingPageOpened()
        service.pairingPageClosed()
        service.pairingPageClosed()
        await Self.phase(of: host, reaches: .standby)
        #expect(await !host.pairing.open)
        #expect(await host.pairing.revision == 2)
        #expect(await endpoint.binds == 1)
        #expect(await endpoint.closes == 1)
    }

    // MARK: Fixtures

    private nonisolated struct Account: MobileHostAuth {
        var projectID: String { "project" }
        var userID: String { "user" }
        var teamID: String { "team" }
        func accessToken(forceRefresh: Bool) async throws -> String { throw NoNetwork() }
        func isCurrent() async -> Bool { true }
    }

    private nonisolated struct NoNetwork: Error {}

    private static let listening = MobileIrxHost.Phase.listening(relayURL: FakeEndpoint.relay)

    /// Returns once `host` is in `target` (at once when it is already).
    private static func phase(of host: MobileIrxHost, reaches target: MobileIrxHost.Phase) async {
        for await phase in await host.phaseUpdates() where phase == target { return }
    }

    /// A listener as provisioning leaves it, on a fake endpoint, with a usable relay credential and `phones` paired phones.
    private static func provisioned(onDemand: Bool, phones: Int = 0, clock: any Clock<Duration> = ContinuousClock())
        async throws -> (MobileIrxHost, FakeEndpoint) {
        let host = try Self.host(onDemand: onDemand, clock: clock)
        let endpoint = FakeEndpoint()
        let tuple = V2Identity(appNamespace: "com.cmuxterm.app.next.test", buildTag: "mobile-bind-test", deviceID: "mac",
                               environment: "development", projectID: "project", teamID: "team", userID: "user")
        let identity = IrxIdentity(privateKeyData: Data(repeating: 7, count: 32), deviceID: "mac", appInstanceID: "mac")
        let descriptor = V2DeviceDescriptor(
            endpointID: identity.endpointIDHex, identity: tuple, identityGeneration: 1,
            metadata: V2DeviceMetadata(appVersion: "0", capabilities: ["irx-v2"], displayName: "Mac", pairingEnabled: true,
                                       platform: .mac, relayURLs: []))
        let admission = try V2InboundAdmissionAuthority(host: descriptor)
        var cache = V2CachedState(identity: tuple)
        let now = Int(Date().timeIntervalSince1970)
        cache.relayCredentials = [V2RelayCredential(expiresAt: now + 3_600, refreshAfter: now + 1_800,
                                                    relayURL: FakeEndpoint.relay, token: "token")]
        cache.directory = try Self.directory(Array(repeating: (platform: "ios", revoked: false), count: phones))
        await host.adopt(backend: EmptyBackend(), identity: identity, admission: admission, deviceID: "mac",
                         endpoint: endpoint, cache: cache)
        return (host, endpoint)
    }

    private static func host(onDemand: Bool, pairingOpen: Bool = false,
                             clock: any Clock<Duration> = ContinuousClock()) throws -> MobileIrxHost {
        let state = FileManager.default.temporaryDirectory.appendingPathComponent("cmux-mobile-bind-\(UUID().uuidString)")
        let configuration = MobileHostConfiguration(
            baseURL: try #require(MobileHostConfiguration.baseURL(environment: "development")), environment: "development",
            namespace: "com.cmuxterm.app.next.test", tag: "mobile-bind-test", stateDirectory: state, keyStorage: .files,
            displayName: "Mac", appVersion: "0", appBuild: "0", daemonSocketPath: { nil })
        return MobileIrxHost(configuration: configuration, auth: Account(), work: BackgroundWork(label: "test.mobile-bind"),
                             makeBackend: { throw NoNetwork() }, clock: clock, endpointOnDemand: onDemand,
                             pairingOpen: pairingOpen)
    }

    /// An account directory whose inbound peers have these platforms and revoked flags.
    private static func directory(_ peers: [(platform: String, revoked: Bool)]) throws -> V2Directory {
        let entries = peers.enumerated().map { index, peer in
            """
            {"device": {"descriptor": {
                "identity": {"environment": "development", "projectId": "project", "teamId": "team", "userId": "user",
                             "deviceId": "device-\(index)", "appNamespace": "dev.cmux.app", "buildTag": "default"},
                "endpointId": "\(String(repeating: "a", count: 63))\(index % 10)", "identityGeneration": 1,
                "metadata": {"platform": "\(peer.platform)", "displayName": "Device \(index)", "appVersion": "1",
                             "pairingEnabled": true, "capabilities": ["irx-v2"], "relayURLs": []}},
              "deviceRecordId": "record-\(index)", "revision": 1, "revoked": \(peer.revoked)},
             "permissionExpiresAt": 2000}
            """
        }
        let wire = """
        {"devices": [], "inboundPeers": [\(entries.joined(separator: ","))], "issuedAt": 1000, "nextCursor": null,
         "permissionExpiresAt": 1900, "relayURLs": [], "revision": 1, "teamId": "team"}
        """
        return try JSONDecoder().decode(V2Directory.self, from: Data(wire.utf8))
    }
}

/// The listener's endpoint, in memory: a bind is online at once, an accept waits until the endpoint closes.
/// A test can watch a step, and hold it there until it opens it again.
private actor FakeEndpoint: MobileHostEndpoint {
    nonisolated static let relay = "https://relay.test"

    nonisolated enum Step: Hashable, Sendable { case bind, relay, accept }

    /// Endpoints bound: a bind while one is bound keeps it, as `IrxEndpointSupervisor.readyEndpoint` does.
    private(set) var binds = 0
    private(set) var closes = 0
    private(set) var accepts = 0
    private(set) var bound = false
    private var acceptors: [AsyncStream<Never>.Continuation] = []
    private var watchers: [Step: AsyncStream<Void>.Continuation] = [:]
    private var held: Set<Step> = []
    private var gates: [Step: AsyncStream<Never>.Continuation] = [:]

    /// Yields each time `step` is reached.
    func watch(_ step: Step) -> AsyncStream<Void> {
        let (stream, continuation) = AsyncStream.makeStream(of: Void.self)
        watchers[step] = continuation
        return stream
    }

    /// From now on, `step` waits where it is reached until ``open(_:)``.
    func hold(_ step: Step) { held.insert(step) }

    func open(_ step: Step) {
        held.remove(step)
        gates.removeValue(forKey: step)?.finish()
    }

    private func reach(_ step: Step) async {
        watchers[step]?.yield(())
        guard held.contains(step) else { return }
        let (opened, gate) = AsyncStream.makeStream(of: Never.self)
        gates[step] = gate
        for await _ in opened {}
    }

    func bind(credentials: [IrxRelayCredential]) async throws {
        await reach(.bind)
        guard !bound else { return }
        binds += 1
        bound = true
    }

    func homeRelayURL() async -> String? {
        await reach(.relay)
        return bound ? Self.relay : nil
    }

    func acceptNextInbound() async -> IrxEndpointSupervisor.AcceptedInbound? {
        guard bound else { return nil }
        accepts += 1
        let (closed, continuation) = AsyncStream.makeStream(of: Never.self)
        acceptors.append(continuation)
        await reach(.accept)
        for await _ in closed {}
        return nil
    }

    func rotateCredentials(_ credentials: [IrxRelayCredential]) {}

    func close() {
        closes += 1
        unbind()
    }

    func deactivate() { unbind() }

    private func unbind() {
        bound = false
        for acceptor in acceptors { acceptor.finish() }
        acceptors.removeAll()
    }
}

/// A daemon with nothing in it: the bind tests admit no phone.
private nonisolated struct EmptyBackend: MobileCompatBackend {
    private nonisolated struct NoDaemon: Error {}

    func tree() async throws -> DaemonTree { DaemonTree() }
    func treeChanges() -> AsyncStream<Void> { AsyncStream { $0.finish() } }
    func createWorkspace(name: String?) async throws -> WorkspaceKey { throw NoDaemon() }
    func renameWorkspace(_ key: WorkspaceKey, to name: String) async throws { throw NoDaemon() }
    func closeWorkspace(_ key: WorkspaceKey) async throws { throw NoDaemon() }
    func createTerminal(in key: WorkspaceKey, cwd: String?) async throws -> TerminalID? { nil }
    func send(_ surface: SurfaceID, bytes: Data, paste: Bool) async throws { throw NoDaemon() }
    func renameTab(_ surface: SurfaceID, to name: String) async throws { throw NoDaemon() }
    func closeTerminal(_ terminal: TerminalID) async throws { throw NoDaemon() }
    func attach(_ tab: TabSnapshot, generation: DaemonGeneration?, size: CellSize) async throws
        -> any MobileCompatTerminalChannel {
        throw NoDaemon()
    }
}
