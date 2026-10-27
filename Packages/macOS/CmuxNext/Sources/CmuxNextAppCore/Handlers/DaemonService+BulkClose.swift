import CmuxNextDaemon
public import CmuxNextDaemonService

extension DaemonService {
    /// Whether this daemon takes a close gesture over several screens or
    /// workspaces as one `layout.bulk` request (`BulkClosePlan`).
    @MainActor public var servesBulkClose: Bool {
        store.servesStateResources && supports(DaemonCapability.layoutBulk.rawValue)
    }
}
