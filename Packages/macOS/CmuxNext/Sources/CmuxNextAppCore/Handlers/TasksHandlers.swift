public import CmuxNextActions

/// What the Tasks actions ask of the App: its Tasks page. `focus` (a user run) selects the page;
/// automation opens it without changing focus or the view.
@MainActor
public protocol TasksActionHost {
    /// Shows the Tasks page scoped to the person's own tasks (`mine`) or to every task.
    func showTasks(mine: Bool, focus: Bool) throws
    /// Shows the Tasks page in its current scope with a new task started.
    func startNewTask(focus: Bool) throws
}

/// Tasks actions (plans/cmux-next/tasks.md section 14, slice 4): each shows
/// the Tasks page; a user run selects it and sets its scope, automation
/// opens it without changing focus or the view.
@MainActor
public final class TasksHandlers {
    private let host: any TasksActionHost

    public init(host: any TasksActionHost) {
        self.host = host
    }

    public func bind(into registry: ActionRegistry) {
        let host = host
        registry.bind("tasks.show", run: { invocation in
            try host.showTasks(mine: false, focus: invocation.allowsViewChange)
        })
        registry.bind("tasks.new", run: { invocation in
            try host.startNewTask(focus: invocation.allowsViewChange)
        })
        registry.bind("tasks.showMine", run: { invocation in
            try host.showTasks(mine: true, focus: invocation.allowsViewChange)
        })
    }
}
