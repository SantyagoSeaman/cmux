import Foundation

/// Tells acpmux whether the pane shows its chat (p14-energy-118), so acpmux can stop a hidden idle
/// harness and wake it first when it shows (p14-energy-117). Each connection hears the state the pane
/// starts with once its first frame went, then each flip of the pane's probe and each move to another
/// chat. Chrome sends a renderer its first hidden state when it creates it
/// (`RenderViewHostImpl::CreateRenderView`, content/browser/renderer_host/render_view_host_impl.cc),
/// then one one-way IPC per edge (`RenderWidgetHostImpl::WasHidden` / `WasShown`,
/// content/browser/renderer_host/render_widget_host_impl.cc). Nothing here waits: the socket sends
/// after the caller's turn and keeps each session's last state, so a repeat sends nothing.
@MainActor final class AgentPaneVisibilityReporter {
    private weak var model: AgentPaneModel?
    private weak var pacer: AgentPaneFramePacer?

    /// Installs the hooks it reports on: the pacer's seen flips, the transport's first frame of a
    /// connection and the model's session moves.
    init(model: AgentPaneModel, pacer: AgentPaneFramePacer) {
        self.model = model
        self.pacer = pacer
        pacer.onSeenChange = { [weak self] in self?.report() }
        model.transport.onFirstFrameSent = { [weak self] in self?.report() }
        model.onSessionAdopted = { [weak self] previous in self?.sessionChanged(from: previous) }
    }

    /// The open connection whose first frame went (its LocalApp token opened it); nil before.
    private var socket: AcpmuxPaneSocket? {
        guard let transport = model?.transport, transport.sentFirst else { return nil }
        return transport.socket
    }

    /// acpmux hears whether the pane is seen, for the chat it shows now.
    func report() {
        guard let socket, let session = model?.sessionId, let pacer else { return }
        socket.visibilityChanged(session: session, visible: pacer.isSeen)
    }

    /// The pane moved from `previous` (nil: a new tab page) to its current chat: the old one is no
    /// longer shown here, and the new one starts in the pane's state.
    func sessionChanged(from previous: String?) {
        if let previous, let socket { socket.sessionLeft(previous) }
        report()
    }
}

nonisolated extension AcpmuxPaneSocket {
    /// The host's own visibility notification; never a page method (``AcpmuxPaneMethods`` does not list it).
    static let visibilityMethod = "_acpmux/visibility"

    /// The JSON-RPC notification of one state (no id: acpmux sends no reply).
    static func visibilityFrame(session: String, visible: Bool) -> String? {
        let frame: [String: Any] = ["jsonrpc": "2.0", "method": visibilityMethod,
                                    "params": ["sessionId": session, "visible": visible]]
        guard let data = try? JSONSerialization.data(withJSONObject: frame) else { return nil }
        return String(data: data, encoding: .utf8)
    }
}
