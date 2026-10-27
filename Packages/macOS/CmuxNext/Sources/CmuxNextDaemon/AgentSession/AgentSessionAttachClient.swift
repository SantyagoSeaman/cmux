import CmuxNextCompat
public import Foundation
import os

/// One agent chat tab of a session daemon, attached as a stream on a connection to that daemon
/// (`agent-session-attach-v1`, cmux-tui/spec/commands.md "Agent session attach"): how the app
/// shows a chat whose acpmux runs on the daemon's machine.
///
/// With a `shared` carrier (the machine's ready connection, ``DaemonConnection/agentSessionCarrier()``)
/// every agent pane of the machine is a stream on that one connection: no socket, reader thread or
/// handshake of its own. Its events reach the page through a bounded queue
/// (``AgentSessionEventQueue``), never on the reader thread, so a long transcript replay delays
/// neither the tree nor another pane. A client takes a connection of its own (through `endpoint`)
/// only where the shared one cannot carry it: no shared carrier, another pane holds the same tab's
/// attachment there, or the daemon refuses it there (``sharedRefusals``: an older daemon's limit of
/// 8 attachments per connection). While the shared connection reconnects, the client waits for the next
/// one (`shared` is asked with the verb's deadline).
///
/// An attach the client gave up on (closed or detached before the answer, or no answer in time) is
/// detached once the daemon may hold it: nothing is left attached on a connection that outlives the pane.
///
/// Connects on the first verb. A dropped connection ends the attachment (`closed`), and the
/// page reconnects with a fresh client and replays from the newest seq it holds. Nothing here
/// names a session, a folder or a command: the daemon pins the session from its tab record.
public actor AgentSessionAttachClient {
    public typealias EndpointProvider = @Sendable () async throws -> DaemonEndpoint
    /// The machine's shared connection as an agent carrier, waiting up to `limit` while it reconnects;
    /// throws when none is ready in time.
    public typealias CarrierProvider = @Sendable (_ limit: Duration) async throws -> AgentSessionCarrier
    /// One push for the attachment: `agent-session-record`, `-permission` or `-closed`, with
    /// the line's JSON object.
    public typealias EventHandler = @Sendable (_ event: String, _ line: Data) -> Void

    public static let capability = DaemonCapability.agentSessionAttach.rawValue

    /// Refusals of the shared connection that a connection of the client's own does not get: the
    /// per-connection attachment limit, a shared connection the daemon does not trust for agent
    /// verbs, and a tab the shared connection holds in the other mode.
    static let sharedRefusals: Set<String> = ["agent_session.limit", "agent_session.not_trusted", "agent_session.mode_conflict"]

    private let surface: UInt64
    private let mode: Mode
    private let endpoint: EndpointProvider
    private let shared: CarrierProvider?
    private let requestTimeout: Duration
    private let queueLimits: AgentSessionEventQueue.Limits
    private let logger = Logger(subsystem: "com.cmuxterm.app.next", category: "agent-attach")
    /// The connection this client's verbs go on.
    private var carrier: AgentSessionCarrier?
    private var connecting: Task<AgentSessionCarrier, any Error>?
    /// This client's attachment: its stream on `carrier`.
    private var stream: AgentSessionStream?
    /// The stream whose attach (or observe) request waits for its answer: a close or detach meanwhile leaves
    /// its detach to that answer (``openAttachment(_:on:)``).
    private var attaching: AgentSessionStream?
    /// The shared connection cannot carry this client: it uses a connection of its own from now on.
    private var ownConnection = false
    private let handler = HandlerBox()
    private var closed = false
    /// The connected daemon serves `agent-session-start-v1` (read from its own `identify`).
    private var servesStart = false
    /// The connected daemon serves `agent-session-trust-v1`.
    private var servesTrust = false
    /// The connected daemon serves `agent-session-start-keyed-reason-v1`.
    private var servesKeyedReason = false
    /// The connected daemon serves `agent-session-children-v1`.
    private var servesChildren = false

    /// A start that needs the app's person key in that machine's acpmux (cx-0w0jj): `instance`
    /// is the acpmux run the app holds a key for, nil when it holds none.
    public struct PersonKey: Sendable, Equatable {
        public var instance: String?
        public init(instance: String?) { self.instance = instance }
    }

    /// - Parameters:
    ///   - surface: The tab's surface id in the daemon's store.
    ///   - requestTimeout: The deadline of each verb's reply.
    ///   - mode: Attach, or observe read-only.
    ///   - shared: The machine's connection to carry the client; nil gives it a connection of its own.
    ///   - endpoint: Where a connection of the client's own goes.
    public init(surface: UInt64, requestTimeout: Duration = .seconds(10), mode: Mode = .attach,
                shared: CarrierProvider? = nil, endpoint: @escaping EndpointProvider) {
        self.init(surface: surface, requestTimeout: requestTimeout, mode: mode, shared: shared,
                  queueLimits: AgentSessionEventQueue.Limits(), endpoint: endpoint)
    }

    /// As the public init, with the bound of the attachment's event queue (tests).
    init(surface: UInt64, requestTimeout: Duration, mode: Mode, shared: CarrierProvider?,
         queueLimits: AgentSessionEventQueue.Limits, endpoint: @escaping EndpointProvider) {
        self.surface = surface
        self.mode = mode
        self.endpoint = endpoint
        self.shared = shared
        self.requestTimeout = requestTimeout
        self.queueLimits = queueLimits
    }

    deinit {
        // A client dropped without close() lets go of its tab on the shared connection.
        let owns = carrier?.ownsConnection == true
        stream?.end(sendsDetach: !owns)
        if owns { carrier?.transport.close() }
    }

    /// Subscribes and returns the attach page's JSON. Pushes go to `events` from then on.
    public func attach(afterSeq: UInt64?, beforeSeq: UInt64?, limit: Int?, kinds: [String]?,
                       events: @escaping EventHandler) async throws -> Data {
        handler.set(events)
        let params = page(afterSeq, beforeSeq, limit, kinds)
        // On an attached tab the daemon only pages.
        if let stream, stream.isOpen { return try await call(mode.command, params) }
        stream = nil
        let first = try await liveCarrier()
        if let page = try await openAttachment(params, on: first) { return page }
        // The shared connection cannot carry this attachment: a connection of the client's own.
        leaveSharedConnection()
        let own = try await liveCarrier()
        guard let page = try await openAttachment(params, on: own) else {
            throw DaemonError.connectionClosed(reason: "the agent attach connection closed")
        }
        return page
    }

    public func events(afterSeq: UInt64?, beforeSeq: UInt64?, limit: Int?, kinds: [String]?) async throws -> Data {
        try await call("agent-session-events", page(afterSeq, beforeSeq, limit, kinds))
    }

    /// `agent-session-start` (`agent-session-start-v1`): the daemon starts a new chat for the tab
    /// in its own machine's acpmux (policy `ask`) and binds it; returns the new session id. A
    /// daemon that does not serve the verb is refused here (`agent_session.start_unsupported`),
    /// so no start ever falls back to another machine.
    ///
    /// With `personKey` the daemon starts only when its acpmux holds that key, else it refuses with
    /// `agent_session.needs_keyed_start` and its facts in ``AgentSessionAttachError/details``. A
    /// daemon without `agent-session-start-keyed-reason-v1` gets no such param (it would refuse it).
    public func start(cwd: String, harness: String?, homeConfirmed: Bool = false, personKey: PersonKey? = nil) async throws -> String {
        guard mode == .attach else {
            throw AgentSessionAttachError(code: Self.startUnsupported, message: "a read-only agent session cannot start a chat")
        }
        _ = try await liveCarrier()
        guard servesStart else {
            throw AgentSessionAttachError(code: Self.startUnsupported, message: "the daemon does not serve agent session start")
        }
        var params: [String: any Sendable] = ["cwd": cwd]
        if let harness { params["harness"] = harness }
        if homeConfirmed { params["home_confirmed"] = true }
        if let personKey, servesKeyedReason {
            params["require_person_key"] = true
            if let instance = personKey.instance { params["person_instance"] = instance }
        }
        let data = try await call("agent-session-start", params)
        guard let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let session = object["session"] as? String, !session.isEmpty else {
            throw AgentSessionAttachError(code: "agent_session.refused", message: "agent-session-start answered no session")
        }
        return session
    }

    /// `agent-session-trust` (`agent-session-trust-v1`): the trust of `cwd` on the daemon's machine
    /// (acpmux's `acp.trust.get` reply), or with `level` the person's answer recorded there
    /// (`acp.trust.set`). Only the person's own answer is ever sent (the pane's relay).
    public func trust(cwd: String, level: String?, homeConfirmed: Bool = false) async throws -> Data {
        guard mode == .attach else {
            throw AgentSessionAttachError(code: Self.startUnsupported, message: "a read-only agent session cannot change trust")
        }
        _ = try await liveCarrier()
        guard servesTrust else {
            throw AgentSessionAttachError(code: Self.startUnsupported, message: "the daemon does not serve agent session trust")
        }
        var params: [String: any Sendable] = ["cwd": cwd]
        if let level { params["level"] = level }
        if homeConfirmed { params["home_confirmed"] = true }
        return try await call("agent-session-trust", params)
    }

    /// The code ``start(cwd:harness:)`` throws for a daemon without `agent-session-start-v1`.
    public static let startUnsupported = "agent_session.start_unsupported"

    public func prompt(id: String, text: String) async throws -> Data {
        try await call("agent-session-prompt", ["prompt_id": id, "text": text])
    }

    public func cancel() async throws {
        _ = try await call("agent-session-cancel", [:])
    }

    /// `agent-session-permission`, with the person's decision proof of a relayed allow when there
    /// is one (cx-0q7o): the daemon passes it to acpmux unchanged, and acpmux decides.
    public func permission(id: String, option: String, decisionNonce: String? = nil, personDecision: String? = nil) async throws {
        var params: [String: any Sendable] = ["permission_id": id, "option_id": option]
        if let decisionNonce, let personDecision {
            params["decision_nonce"] = decisionNonce
            params["person_decision"] = personDecision
        }
        _ = try await call("agent-session-permission", params)
    }

    /// Ends this client's attachment; no push after it. Sends nothing when it holds none: on a
    /// shared connection the tab's attachment may be another pane's.
    public func detach() async {
        guard let stream else { return }
        self.stream = nil
        if stream === attaching {
            stream.stopDelivery()  // its attach's answer sends the detach
            return
        }
        await stream.detach(timeout: requestTimeout)
    }

    /// `agent-session-children` (`agent-session-children-v1`): the chats the attached tab's session
    /// started on the daemon's machine (`{sessions}`, acpmux summaries); the attachment then
    /// streams their changes too. A daemon without the verb lists none.
    public func children() async throws -> Data {
        _ = try await liveCarrier()
        guard servesChildren else { return Data(#"{"sessions":[]}"#.utf8) }
        return try await call("agent-session-children", [:])
    }

    /// Reads one image from the attached session's host acpmux (`agent-session-media`).
    public func media(path: String) async throws -> Data {
        try await call("agent-session-media", ["path": path])
    }

    /// Ends the attachment, and the connection when it is the client's own; no push after it.
    public func close() {
        closed = true
        handler.set(nil)
        connecting?.cancel()
        connecting = nil
        let owns = carrier?.ownsConnection == true
        if let stream, stream === attaching {
            stream.stopDelivery()  // its attach's answer sends the detach
        } else {
            stream?.end(sendsDetach: !owns)
        }
        stream = nil
        if owns { carrier?.transport.close() }
        carrier = nil
    }

    // MARK: Private

    private func page(_ after: UInt64?, _ before: UInt64?, _ limit: Int?, _ kinds: [String]?) -> [String: any Sendable] {
        var params: [String: any Sendable] = [:]
        if let after { params["after_seq"] = after }
        if let before { params["before_seq"] = before }
        if let limit { params["limit"] = limit }
        if let kinds { params["kinds"] = kinds }
        return params
    }

    /// Opens this client's stream on `carrier` and sends the attach (or observe). Nil when the
    /// carrier cannot carry it: another stream holds the tab there, or a shared connection's refusal
    /// (``sharedRefusals``). The stream is registered before the request, so the records that follow
    /// the reply reach it.
    ///
    /// Only the daemon's refusal says it attached nothing. An answer that comes after this client let
    /// go of the stream (close, detach), and a request with no answer (a timeout), may leave an
    /// attachment on the daemon: the stream detaches it, still holding its surface until then.
    private func openAttachment(_ params: [String: any Sendable], on carrier: AgentSessionCarrier) async throws -> Data? {
        let handler = handler
        let opened = AgentSessionStream.open(surface: surface, on: carrier.transport, limits: queueLimits) { name, line in
            handler.current?(name, line)
            if name == AgentSessionStream.closedEvent { handler.set(nil) }
        }
        guard let opened else {
            if carrier.transport.isClosed { throw DaemonError.connectionClosed(reason: "the agent attach connection closed") }
            return nil
        }
        stream = opened
        attaching = opened
        defer { if attaching === opened { attaching = nil } }
        let page: Data
        do {
            page = try await call(mode.command, params, on: carrier)
        } catch let refusal as AgentSessionAttachError {
            let abandoned = stream !== opened || closed
            forget(opened, sendsDetach: false)
            guard !abandoned, !carrier.ownsConnection, Self.sharedRefusals.contains(refusal.code) else { throw refusal }
            logger.info("agent attach refused on the shared connection: \(refusal.code, privacy: .public)")
            return nil
        } catch {
            forget(opened, sendsDetach: true)
            throw error
        }
        guard stream === opened, !closed else {
            opened.end(sendsDetach: true)
            throw AgentSessionAttachError(code: "agent_session.closed", message: "closed before the attach answered")
        }
        return page
    }

    /// Ends a stream whose attach did not complete; `sendsDetach` when the daemon may hold it.
    private func forget(_ dropped: AgentSessionStream, sendsDetach: Bool) {
        dropped.end(sendsDetach: sendsDetach)
        if stream === dropped { stream = nil }
    }

    /// From now on this client's verbs go on a connection of its own; the shared one stays open.
    private func leaveSharedConnection() {
        ownConnection = true
        if carrier?.ownsConnection == false { carrier = nil }
    }

    /// One verb on the tab; the result's `data` as JSON, or the daemon's refusal. A verb the shared
    /// connection is not trusted for, sent with no attachment there, goes on a connection of the client's own.
    private func call(_ cmd: DaemonCommandName, _ params: [String: any Sendable]) async throws -> Data {
        let current = try await liveCarrier()
        do {
            return try await call(cmd, params, on: current)
        } catch let refusal as AgentSessionAttachError where !current.ownsConnection && stream == nil
                    && refusal.code == "agent_session.not_trusted" {
            leaveSharedConnection()
            return try await call(cmd, params, on: try await liveCarrier())
        }
    }

    private func call(_ cmd: DaemonCommandName, _ params: [String: any Sendable], on carrier: AgentSessionCarrier) async throws -> Data {
        var object = params
        object["cmd"] = cmd.wire
        object["surface"] = surface
        // Integers stay integers (the daemon reads u64 seqs and surface ids).
        let fields = try JSONSerialization.data(withJSONObject: object)
        let response: LineTransport.Response
        do {
            response = try await carrier.transport.request(cmd: cmd, timeout: requestTimeout) { id in
                Data(#"{"id":\#(id),"#.utf8) + fields.dropFirst()
            }
        } catch DaemonError.command(_, let message, let code, let details, _) {
            throw AgentSessionAttachError(code: code ?? "agent_session.refused", message: message, details: details)
        }
        response.recorder?.decoded()  // Self.data(of:) parses the line
        return Self.data(of: response.line)
    }

    private func liveCarrier() async throws -> AgentSessionCarrier {
        if closed { throw AgentSessionAttachError(code: "agent_session.closed", message: "closed") }
        if let carrier, !carrier.transport.isClosed { return carrier }
        carrier = nil
        if let connecting { return try await connecting.value }
        let task = Task { try await self.connect() }
        connecting = task
        defer { connecting = nil }
        let connected = try await task.value
        if closed {
            if connected.ownsConnection { connected.transport.close() }
            throw AgentSessionAttachError(code: "agent_session.closed", message: "closed")
        }
        carrier = connected
        return connected
    }

    /// The machine's shared connection when there is one and the client may use it (waiting for it
    /// while it reconnects), else a connection of the client's own.
    private func connect() async throws -> AgentSessionCarrier {
        if !ownConnection, let shared {
            let carrier = try await shared(requestTimeout)
            try adopt(carrier.identity)
            return carrier
        }
        return try await connectOwn()
    }

    /// Reads what the daemon serves; throws when it does not serve this client's mode.
    private func adopt(_ identity: DaemonIdentity) throws {
        guard identity.supports(mode.capability) else {
            throw AgentSessionAttachError(code: "agent_session.unsupported", message: mode.unsupportedMessage)
        }
        servesStart = identity.supports(DaemonCapability.agentSessionStart.rawValue)
        servesTrust = identity.supports(DaemonCapability.agentSessionTrust.rawValue)
        servesKeyedReason = identity.supports(DaemonCapability.agentSessionStartKeyedReason.rawValue)
        servesChildren = identity.supports(DaemonCapability.agentSessionChildren.rawValue)
    }

    /// A connection of this client's own. Its agent events route to the client's stream as the
    /// shared connection's do (``LineTransport/routeAgentSession(name:surface:line:)``).
    private func connectOwn() async throws -> AgentSessionCarrier {
        let endpoint = try await self.endpoint()
        let transport = try await LineTransport.open(path: endpoint.socketPath, bridge: endpoint.bridge,
                                                     role: .agentSessionAttach, stats: endpoint.wireStats)
        transport.start(onEvent: { _, _, _ in }, onClose: { [logger] reason in
            logger.info("agent attach connection ended: \(String(describing: reason), privacy: .public)")
        })
        do {
            let identity = try await DaemonConnection.perform(IdentifyRequest(), on: transport, timeout: requestTimeout)
            try adopt(identity)
            _ = try await DaemonConnection.perform(
                SetClientInfoRequest(name: mode.clientInfoName, kind: mode.clientInfoKind, capabilities: []),
                on: transport, timeout: requestTimeout)
            return AgentSessionCarrier(transport: transport, identity: identity, ownsConnection: true)
        } catch {
            transport.close()
            throw error
        }
    }

    /// The `data` member of an `ok:true` response line, as JSON.
    static func data(of line: Data) -> Data {
        guard let object = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any],
              let data = object["data"], JSONSerialization.isValidJSONObject(data),
              let encoded = try? JSONSerialization.data(withJSONObject: data) else { return Data("{}".utf8) }
        return encoded
    }
}

/// The current push handler, read by the attachment's event queue.
private final class HandlerBox: Sendable {
    private let lock = Mutex<AgentSessionAttachClient.EventHandler?>(nil)
    var current: AgentSessionAttachClient.EventHandler? { lock.withLock { $0 } }
    func set(_ handler: AgentSessionAttachClient.EventHandler?) { lock.withLock { $0 = handler } }
}
