import CmuxNextBrowserImport

/// Why a parse through the sandboxed service failed. Each fails only the profile it was for, as a
/// reader error does in process; the service ending mid-parse is one of them, never the app's end
/// (Chrome's utility process model: a crashed or misbehaving child loses its own job).
public enum BrowserImportServiceError: Error, Sendable, Equatable, CustomStringConvertible {
    /// The connection failed: the service did not start, ended, or was ended. The `NSXPCConnection`
    /// error code.
    case connection(Int)
    /// The reply was not a value of the wire's types: the service is not trusted for this parse.
    case badReply
    /// The service answered with a failure.
    case service(BrowserImportServiceFailure)

    public var description: String {
        switch self {
        case .connection(let code): "the import service ended (XPC error \(code))"
        case .badReply: "the import service sent a reply that does not read"
        case .service(let failure): failure.description
        }
    }
}

extension ProfileParseResult {
    /// Whether this is the kind of value `format`'s reader gives.
    func matches(_ format: ProfileParseSpec.Format) -> Bool {
        switch (self, format) {
        case (.bookmarks, .chromiumBookmarks), (.bookmarks, .safariBookmarks), (.bookmarks, .arcSidebar), (.bookmarks, .firefoxBookmarks),
             (.history, .chromiumHistory), (.history, .firefoxHistory), (.history, .safariHistory),
             (.openTabs, .chromiumSession), (.openTabs, .firefoxSession):
            true
        default:
            false
        }
    }
}
