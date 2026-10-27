import CmuxNextActions
import CmuxNextAppCore
import CmuxNextDaemonService
import Foundation
import Testing

/// The handlers moved from CmuxNextApp to CmuxNextAppCore behind host protocols (p15-oracle-163):
/// each catalog action still binds, asks its host what it asked the App's services before the move,
/// passes the run's focus rule, and refuses a thrown error as the App's `bind(run:)` does.
@MainActor @Suite struct AppCoreHandlerMovesTests {
    private struct Thrown: Error, CustomStringConvertible {
        var description: String { "boom" }
    }

    @MainActor private final class Host: TasksActionHost, TaskManagerActionHost, FilesPaneActionHost {
        enum Call: Equatable {
            case tasks(mine: Bool, focus: Bool)
            case newTask(focus: Bool)
            case taskManager(focus: Bool)
            case kill(String?)
            case files(URL?, focus: Bool)
        }

        var calls: [Call] = []
        var folder: URL?? = .some(nil)
        var failure: (any Error)?

        func showTasks(mine: Bool, focus: Bool) throws {
            calls.append(.tasks(mine: mine, focus: focus))
            if let failure { throw failure }
        }

        func startNewTask(focus: Bool) throws {
            calls.append(.newTask(focus: focus))
            if let failure { throw failure }
        }

        func showTaskManager(focus: Bool) throws {
            calls.append(.taskManager(focus: focus))
            if let failure { throw failure }
        }

        func killProcess(_ process: String?) throws {
            calls.append(.kill(process))
            if let failure { throw failure }
        }

        func openAsPaneFolder(_ invocation: ActionInvocation) -> URL?? { folder }

        func openFilesPane(folder: URL?, focus: Bool) throws {
            calls.append(.files(folder, focus: focus))
            if let failure { throw failure }
        }
    }

    private static let movedIDs: [ActionID] = [
        "tasks.show", "tasks.new", "tasks.showMine", "palette.openTaskManager", "taskManager.killProcess",
        "files.open", "palette.openFilesPane",
    ]

    private func bound() -> (ActionRegistry, Host) {
        let registry = ActionRegistry(catalog: ActionCatalog.all, aliases: ActionCatalog.legacyAliases)
        let host = Host()
        TasksHandlers(host: host).bind(into: registry)
        TaskManagerHandlers(host: host).bind(into: registry)
        FilesPaneHandlers(host: host).bind(into: registry)
        return (registry, host)
    }

    @Test func everyMovedActionIsInTheCatalogAndBound() {
        let (registry, _) = bound()
        for id in Self.movedIDs {
            #expect(registry.descriptor(for: id) != nil, "\(id) left the catalog")
            #expect(registry.isBound(id), "\(id) is not bound after the move")
        }
    }

    @Test func tasksActionsShowTheirViewAndFocusOnlyForAPerson() {
        let (registry, host) = bound()
        registry.perform("tasks.show", invocation: ActionInvocation(origin: .user))
        registry.perform("tasks.new", invocation: ActionInvocation(origin: .cli))
        registry.perform("tasks.showMine", invocation: ActionInvocation(origin: .script))
        #expect(host.calls == [.tasks(mine: false, focus: true), .newTask(focus: false), .tasks(mine: true, focus: false)])
    }

    @Test func taskManagerOpensAndKillsTheNamedProcess() {
        let (registry, host) = bound()
        registry.perform("palette.openTaskManager", invocation: ActionInvocation(origin: .user))
        let confirmed = ActionInvocation(arguments: ["process": .string("4242"), ActionArgument.confirmName: .bool(true)], origin: .cli)
        registry.perform("taskManager.killProcess", invocation: confirmed)
        #expect(host.calls == [.taskManager(focus: true), .kill("4242")])
    }

    @Test func openFilesAsPaneOpensOnTheHostsFolderAndStopsWhenTheNamedPaneIsGone() {
        let (registry, host) = bound()
        let folder = URL(fileURLWithPath: "/tmp/project", isDirectory: true)
        registry.perform("files.open", invocation: ActionInvocation(origin: .cli))
        host.folder = .some(folder)
        registry.perform("palette.openFilesPane", invocation: ActionInvocation(origin: .user))
        host.folder = .some(nil)
        registry.perform("palette.openFilesPane", invocation: ActionInvocation(origin: .mcp))
        host.folder = .none
        registry.perform("palette.openFilesPane", invocation: ActionInvocation(origin: .user))
        #expect(host.calls == [.files(nil, focus: false), .files(folder, focus: true), .files(nil, focus: false)])
    }

    @Test func aThrownErrorRefusesAsTheAppsThrowingBindDoes() throws {
        let (registry, host) = bound()
        host.failure = ActionFailure(message: "gone", isNotFound: true)
        let notFound = try #require(registry.capturingTypedRefusal { registry.perform("tasks.show", invocation: ActionInvocation(origin: .cli)) })
        #expect(notFound.reason == "gone" && notFound.isNotFound)
        host.failure = ActionFailure(message: "not here")
        let plain = try #require(registry.capturingTypedRefusal { registry.perform("files.open", invocation: ActionInvocation(origin: .cli)) })
        #expect(plain.reason == "not here" && !plain.isNotFound)
        host.failure = Thrown()
        let other = registry.capturingRefusal { registry.perform("palette.openTaskManager", invocation: ActionInvocation(origin: .cli)) }
        #expect(other == RefusalStrings.describe(Thrown()))
    }

    @Test func aQuietFailureRefusesWithoutANotice() {
        let (registry, host) = bound()
        let seen = Refusals()
        registry.refusalObserver = { reason, quiet in seen.all.append(.init(reason: reason, quiet: quiet)) }
        host.failure = ActionFailure(message: "edge", isQuiet: true)
        registry.perform("tasks.showMine", invocation: ActionInvocation(origin: .user))
        host.failure = ActionFailure(message: "loud")
        registry.perform("tasks.showMine", invocation: ActionInvocation(origin: .user))
        #expect(seen.all == [.init(reason: "edge", quiet: true), .init(reason: "loud", quiet: false)])
    }

    @MainActor private final class Refusals {
        struct Refusal: Equatable {
            var reason: String
            var quiet: Bool
        }

        var all: [Refusal] = []
    }
}
