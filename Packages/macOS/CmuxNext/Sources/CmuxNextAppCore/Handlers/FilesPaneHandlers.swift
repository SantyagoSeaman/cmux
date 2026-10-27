public import CmuxNextActions
public import Foundation

/// What the Files pane actions ask of the App: the folder a run opens on, and its Files tab.
@MainActor
public protocol FilesPaneActionHost {
    /// The folder Open Files as Pane opens on: the run's `path`, else the named or focused pane's
    /// folder. The inner nil means home; the outer nil means the run named a pane no window shows
    /// (it was refused).
    func openAsPaneFolder(_ invocation: ActionInvocation) -> URL??
    /// Opens a Files tab on `folder` (nil: home). `focus` (a user run) selects it.
    func openFilesPane(folder: URL?, focus: Bool) throws
}

/// Files pane actions (u2360): `files.open` opens a new Files tab on the
/// home folder; a user run selects it, automation opens it without moving focus.
/// Open Files as Pane (classic's right sidebar Files tool as a pane) opens the
/// same Files tab on the focused terminal's folder, or the CLI's `--pane` or `--path`.
@MainActor
public final class FilesPaneHandlers {
    private let host: any FilesPaneActionHost

    public init(host: any FilesPaneActionHost) {
        self.host = host
    }

    public func bind(into registry: ActionRegistry) {
        let host = host
        registry.bind("files.open", run: { invocation in
            try host.openFilesPane(folder: nil, focus: invocation.allowsViewChange)
        })
        registry.bind("palette.openFilesPane", run: { invocation in
            guard let folder = host.openAsPaneFolder(invocation) else { return }
            try host.openFilesPane(folder: folder, focus: invocation.allowsViewChange)
        })
    }
}
