import CmuxNextActions
import CmuxNextAppCore
import CmuxNextTasks
import Foundation

// The App's answers to the hosts of the handlers in CmuxNextAppCore (p15-oracle-163): each forwards to the page
// service the handler drove before it moved.

extension AppServices: TasksActionHost {
    func showTasks(mine: Bool, focus: Bool) throws { try tasks.show(scope: mine ? .mine : .all, focus: focus) }
    func startNewTask(focus: Bool) throws { try tasks.show(scope: nil, newTask: true, focus: focus) }
}

extension AppServices: TaskManagerActionHost {
    func showTaskManager(focus: Bool) throws { try taskManager.show(focus: focus) }
    func killProcess(_ process: String?) throws { try taskManager.kill(taskManager.process(process)) }
}

extension AppServices: FilesPaneActionHost {
    func openAsPaneFolder(_ invocation: ActionInvocation) -> URL?? { AppActionContext(services: self).openAsPaneFolder(invocation) }
    func openFilesPane(folder: URL?, focus: Bool) throws { try viewers.filesPanes.open(folder: folder, focus: focus) }
}
