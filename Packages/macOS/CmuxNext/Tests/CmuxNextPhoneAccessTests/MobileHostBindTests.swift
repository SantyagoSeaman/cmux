import CmuxIrxTransport
@testable import CmuxNextMobile
@testable import CmuxNextPhoneAccess
import CmuxNextWakeups
import Foundation
import Testing

/// Phone Access binds its iroh endpoint only when a phone can use it (p14-energy-122,
/// `phoneEndpointOnDemand`): no phone, no endpoint (no relay connection, no IrohLib thread);
/// the pairing page or a live attach ticket binds one; a paired phone keeps it.
struct MobileHostBindTests {
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
        #expect(throws: MobileIrxHost.TicketFailure.self) {
            _ = try await host.attachTicket(ttl: 600, target: .physicalDevice, scheme: nil)
        }
        #expect(await host.ticketHold != nil)
        #expect(await host.endpointWanted)
        await host.stop()
        #expect(await host.ticketHold == nil)
    }

    @Test func offOnDemandATicketHoldsNothing() async throws {
        let host = try Self.host(onDemand: false)
        #expect(throws: MobileIrxHost.TicketFailure.self) {
            _ = try await host.attachTicket(ttl: 600, target: .physicalDevice, scheme: nil)
        }
        #expect(await host.ticketHold == nil)
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

    // MARK: Fixtures

    private nonisolated struct Account: MobileHostAuth {
        var projectID: String { "project" }
        var userID: String { "user" }
        var teamID: String { "team" }
        func accessToken(forceRefresh: Bool) async throws -> String { throw NoNetwork() }
        func isCurrent() async -> Bool { true }
    }

    private nonisolated struct NoNetwork: Error {}

    private static func host(onDemand: Bool, pairingOpen: Bool = false) throws -> MobileIrxHost {
        let state = FileManager.default.temporaryDirectory.appendingPathComponent("cmux-mobile-bind-\(UUID().uuidString)")
        let configuration = MobileHostConfiguration(
            baseURL: try #require(MobileHostConfiguration.baseURL(environment: "development")), environment: "development",
            namespace: "com.cmuxterm.app.next.test", tag: "mobile-bind-test", stateDirectory: state, keyStorage: .files,
            displayName: "Mac", appVersion: "0", appBuild: "0", daemonSocketPath: { nil })
        return MobileIrxHost(configuration: configuration, auth: Account(), work: BackgroundWork(label: "test.mobile-bind"),
                             makeBackend: { throw NoNetwork() }, endpointOnDemand: onDemand, pairingOpen: pairingOpen)
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
