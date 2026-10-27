public import CmuxIrxTransport

/// The phone listener's iroh endpoint as ``MobileIrxHost`` drives it: bind, accept, close.
/// The app's is an ``IrxEndpointSupervisor`` (``SupervisedEndpoint``); tests pass a fake.
public protocol MobileHostEndpoint: Sendable {
    /// Binds the endpoint on these relay credentials, or keeps the one already online.
    func bind(credentials: [IrxRelayCredential]) async throws
    /// The relay the bound endpoint homes on; nil when none is bound.
    func homeRelayURL() async -> String?
    /// The next inbound connection, or nil once the endpoint closes.
    func acceptNextInbound() async -> IrxEndpointSupervisor.AcceptedInbound?
    func rotateCredentials(_ credentials: [IrxRelayCredential]) async
    /// Closes the bound endpoint and its relay connection; a later ``bind(credentials:)`` binds a new one.
    func close() async
    /// Closes for good (stop, sign-out).
    func deactivate() async
}

/// An ``IrxEndpointSupervisor`` as the listener's endpoint.
struct SupervisedEndpoint: MobileHostEndpoint {
    let supervisor: IrxEndpointSupervisor

    func bind(credentials: [IrxRelayCredential]) async throws {
        _ = try await supervisor.readyEndpoint(credentials: credentials)
    }

    func homeRelayURL() async -> String? { await supervisor.homeRelayURL() }
    func acceptNextInbound() async -> IrxEndpointSupervisor.AcceptedInbound? { await supervisor.acceptNextInbound() }
    func rotateCredentials(_ credentials: [IrxRelayCredential]) async { await supervisor.rotateCredentials(credentials) }
    func close() async { await supervisor.close() }
    func deactivate() async { await supervisor.deactivate() }
}
