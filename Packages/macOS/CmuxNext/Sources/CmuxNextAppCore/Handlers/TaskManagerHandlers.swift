public import CmuxNextActions

/// What the Task Manager actions ask of the App: its Task Manager page.
@MainActor
public protocol TaskManagerActionHost {
    /// Shows the Task Manager page; `focus` (a user run) selects it.
    func showTaskManager(focus: Bool) throws
    /// Kills the process `process` names (the run's `process` argument, nil when it has none);
    /// throws when that names no listed process.
    func killProcess(_ process: String?) throws
}

/// Task Manager (`palette.openTaskManager`: palette, Window menu, `cmux app
/// task-manager`) and Kill Process (`taskManager.killProcess`: the page's
/// row menu and button, `cmux app kill-process <pid> --confirm`). Kill
/// Process is destructive, so every run asks first or needs `confirm`.
@MainActor
public final class TaskManagerHandlers {
    private let host: any TaskManagerActionHost

    public init(host: any TaskManagerActionHost) {
        self.host = host
    }

    public func bind(into registry: ActionRegistry) {
        let host = host
        registry.bind("palette.openTaskManager", run: { invocation in
            try host.showTaskManager(focus: invocation.allowsViewChange)
        })
        registry.bind("taskManager.killProcess", run: { invocation in
            try host.killProcess(invocation["process"]?.stringValue)
        })
    }
}
