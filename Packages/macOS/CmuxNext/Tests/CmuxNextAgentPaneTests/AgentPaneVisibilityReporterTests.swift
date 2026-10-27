import Foundation
import Testing
@testable import CmuxNextAgentPane

/// p14-energy-118 on the pane's path: the pacer's probe flips, the transport's connections and the
/// model's session moves reach acpmux through ``AgentPaneVisibilityReporter`` (the hooks the pane
/// installs). Each connection hears the pane's state once its first frame went, then each edge.
@Suite(.serialized)
@MainActor
struct AgentPaneVisibilityReporterTests {
    typealias Wire = AgentPaneVisibilityTests.Wire

    private actor NoHost: AgentPaneHostProviding {
        func handshake(sessionId: String?) async throws -> AgentPaneHandshake { throw AgentPaneHostError.daemonStopped }
    }

    static let initialize = #"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#

    /// A pane: its model and transport, a pacer whose probe the test flips, and the reporter the view installs.
    final class Rig {
        var hidden: Bool
        let transport = AgentPaneTransport()
        let model: AgentPaneModel
        lazy var pacer = AgentPaneFramePacer(
            frames: AgentPaneHiddenPacingTests.Ticks(), fallback: AgentPaneHiddenPacingTests.Deadline(),
            nextTurn: { _ in }, isHidden: { [unowned self] in self.hidden })
        var reporter: AgentPaneVisibilityReporter?
        /// Connections whose first frame went (the reporter heard it), and who waits for the next.
        var firstFrames = 0
        var firstFrameWaiter: CheckedContinuation<Void, Never>?

        init(hidden: Bool, session: String?) {
            self.hidden = hidden
            model = AgentPaneModel(host: NoHost(), sessionId: session, transport: transport)
            reporter = AgentPaneVisibilityReporter(model: model, pacer: pacer)
            let reported = transport.onFirstFrameSent
            transport.onFirstFrameSent = { [unowned self] in
                reported?()
                self.firstFrames += 1
                self.firstFrameWaiter?.resume()
                self.firstFrameWaiter = nil
            }
        }

        /// The probe's node flipped to `hidden`.
        func flip(hidden: Bool) {
            self.hidden = hidden
            pacer.visibilityChanged()
        }

        /// Opens a connection on `wire`: its id and its socket.
        func open(_ wire: Wire) async throws -> (Int, AcpmuxPaneSocket) {
            let id = try await transport.open(.remote(AgentPaneRemoteRoute(machine: "m") { wire }))
            return (id, try #require(transport.socket))
        }

        /// Sends the page's first frame on `connection` and waits until the reporter heard it went.
        func sendFirst(_ connection: Int) async {
            let expected = firstFrames + 1
            _ = await transport.send(connection: connection, frames: [AgentPaneVisibilityReporterTests.initialize])
            guard firstFrames < expected else { return }
            await withCheckedContinuation { firstFrameWaiter = $0 }
        }

        /// The visibility frames `wire` got, once every state handed to its `socket` went.
        func edges(_ wire: Wire, _ socket: AcpmuxPaneSocket) async -> [String] {
            await AgentPaneVisibilityTests.outboundDone(socket)
            return wire.frames.compactMap { AgentPaneVisibilityTests.visibility($0.text) }.map { "\($0.0):\($0.1)" }
        }
    }

    /// A chat restored in a background workspace: the flips before the first frame send nothing, and the
    /// first frame sends its hidden state once, so acpmux can freeze it.
    @Test func aPaneHiddenBeforeItsFirstFrameSendsOneHiddenEdge() async throws {
        let rig = Rig(hidden: true, session: "s")
        rig.flip(hidden: true)
        let wire = Wire()
        let (id, socket) = try await rig.open(wire)
        rig.flip(hidden: true)
        rig.flip(hidden: false)
        rig.flip(hidden: true)
        #expect(wire.frames.isEmpty, "a visibility frame went before the page's first frame")

        await rig.sendFirst(id)
        #expect(await rig.edges(wire, socket) == ["s:false"])
    }

    /// The acceptance: after the first frame, hide, hide, show sends exactly two edges.
    @Test func hideHideShowOnThePaneSendsTwoEdges() async throws {
        let rig = Rig(hidden: false, session: "s")
        let wire = Wire()
        let (id, socket) = try await rig.open(wire)
        await rig.sendFirst(id)
        #expect(await rig.edges(wire, socket) == ["s:true"])

        rig.flip(hidden: true)
        rig.flip(hidden: true)
        rig.flip(hidden: false)
        #expect(await rig.edges(wire, socket) == ["s:true", "s:false", "s:true"])
    }

    /// A frozen hidden page's connection closed; the pane shows while it reconnects. The show went to no
    /// connection, so the new one hears it with its first frame.
    @Test func aReconnectWhileShownSendsVisibleOnTheNewConnection() async throws {
        let rig = Rig(hidden: false, session: "s")
        let first = Wire()
        let (id, socket) = try await rig.open(first)
        await rig.sendFirst(id)
        rig.flip(hidden: true)
        #expect(await rig.edges(first, socket) == ["s:true", "s:false"])

        rig.transport.close(connection: id)
        rig.flip(hidden: false)
        #expect(await rig.edges(first, socket) == ["s:true", "s:false"])

        let second = Wire()
        let (next, nextSocket) = try await rig.open(second)
        #expect(second.frames.isEmpty)
        await rig.sendFirst(next)
        #expect(await rig.edges(second, nextSocket) == ["s:true"])
    }

    /// The quick panel moves a shown pane from a to b: a is no longer shown here, b is.
    @Test func aSessionMoveHidesTheOldChatAndShowsTheNewOne() async throws {
        let rig = Rig(hidden: false, session: "a")
        let wire = Wire()
        let (id, socket) = try await rig.open(wire)
        await rig.sendFirst(id)
        rig.model.adoptSession("b")
        #expect(await rig.edges(wire, socket) == ["a:true", "a:false", "b:true"])

        // Hidden, it moves back: the flip told b hidden, and a was told hidden when the pane left it.
        rig.flip(hidden: true)
        rig.model.adoptSession("a")
        #expect(await rig.edges(wire, socket) == ["a:true", "a:false", "b:true", "b:false"])
    }

    /// A new tab page has no chat: its edges wait, and the chat it starts hears the pane's state.
    @Test func aNewTabPagesChatHearsThePaneStateWhenItStarts() async throws {
        let rig = Rig(hidden: false, session: nil)
        let wire = Wire()
        let (id, socket) = try await rig.open(wire)
        await rig.sendFirst(id)
        rig.flip(hidden: true)
        #expect(await rig.edges(wire, socket).isEmpty)

        rig.model.adoptSession("b")
        #expect(await rig.edges(wire, socket) == ["b:false"])
    }
}
