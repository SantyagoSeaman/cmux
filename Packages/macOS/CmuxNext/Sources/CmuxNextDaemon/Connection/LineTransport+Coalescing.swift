import CmuxNextIPC
import Foundation

// Continuous requests on a LineTransport (p05-ipc-141): a newer one replaces the unsent one with its key, and the
// replaced request stops waiting for a reply.

extension LineTransport {
    /// Writes `line`, request `id`'s, with the socket lock held; with a `key`, as a continuous request
    /// (``FrameWriter/enqueue(_:priority:continuous:)``) whose replaced predecessor leaves the pending requests and
    /// the deadlines at once: it will never be written, so nothing will ever answer its id. Returns why the write
    /// failed.
    func writeLine(_ line: Data, id: UInt64, coalescing key: FrameWriter.CoalesceKey?) -> String? {
        guard let key else { return writer.write(line)?.description }
        switch writer.write(line, continuous: FrameWriter.Continuous(key: key, identity: id)) {
        case .queued:
            return nil
        case .replaced(let replaced):
            state.withLock { state in
                _ = state.pending.remove(id: replaced)
                removeDeadline(&state, id: replaced)
            }
            return nil
        case .refused(let failure):
            return failure.description
        }
    }

    /// Requests that wait for a reply (tests).
    var pendingRequests: Int { state.withLock { $0.pending.count } }
}
