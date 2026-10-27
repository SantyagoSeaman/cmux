/// Why a parse through the sandboxed service failed. Each fails only the profile it was for, as a
/// reader error does; the service ending mid-parse is one of them, never the app's end (Chrome's
/// utility process model: a crashed or misbehaving child loses its own job).
public enum BrowserImportServiceError: Error, Sendable, Equatable, CustomStringConvertible {
    /// This build has no import service bundle (`Contents/XPCServices`), so nothing can be parsed.
    case notBundled
    /// The connection failed: the service did not start, ended, or was ended. The `NSXPCConnection`
    /// error code.
    case connection(Int)
    /// The reply was not a value of the wire's types, or broke a reader's invariant: the service is
    /// not trusted for this parse.
    case badReply
    /// The service answered with a failure.
    case service(BrowserImportServiceFailure)

    public var description: String {
        switch self {
        case .notBundled: "this build has no browser import service"
        case .connection(let code): "the import service ended (XPC error \(code))"
        case .badReply: "the import service sent a reply that does not read"
        case .service(let failure): failure.description
        }
    }
}
