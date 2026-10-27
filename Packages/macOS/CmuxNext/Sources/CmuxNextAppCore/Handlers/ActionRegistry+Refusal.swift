public import CmuxNextActions
import CmuxNextDaemonService

extension ActionRegistry {
    /// Reports an error a handler threw; the one mapping for every throwing bind. A not-found
    /// ``ActionFailure`` answers `not_found`, a quiet one refuses with no notice, any other error
    /// refuses with its described reason (`RefusalStrings.describe`).
    @MainActor
    public func refuse(thrown error: any Error) {
        if let failure = error as? ActionFailure, failure.isNotFound {
            refuseNotFound(failure.message)
        } else if let failure = error as? ActionFailure, failure.isQuiet {
            refuse(failure.message, quiet: true)
        } else {
            refuse(RefusalStrings.describe(error))
        }
    }

    /// Binds a throwing handler whose error is reported with ``refuse(thrown:)``: the App's
    /// `bind(_:requires:daemon:unavailable:run:)` without its capability gate, for the handlers in this module.
    @MainActor @discardableResult
    func bind(_ id: ActionID, unavailable: @escaping @MainActor () -> String? = { nil },
              run: @escaping @MainActor (ActionInvocation) throws -> Void) -> Bool {
        bind(id, unavailable: unavailable, invoke: { [weak self] invocation in
            do {
                try run(invocation)
            } catch {
                self?.refuse(thrown: error)
            }
        })
    }
}
