public import Foundation

/// The sandboxed import service's XPC interface (plans/cmux-next/rule-of-2.md): one parse per call.
/// `request` is a binary property list of a `ProfileParseSpec`. `main` is the file to parse, opened
/// read-only by the app; `wal` and `journal` are a database's side files, when it has them. The
/// service has no file access of its own and never sees a path. `reply` gets a binary property list
/// of a ``BrowserImportServiceReply``.
@objc public protocol BrowserImportParsing {
    func parse(_ request: Data, main: FileHandle, wal: FileHandle?, journal: FileHandle?,
               reply: @escaping @Sendable (Data) -> Void)
}

extension NSXPCInterface {
    /// The interface both ends set: the app as the remote object's, the service as its exported one.
    package static func browserImportParsing() -> NSXPCInterface {
        NSXPCInterface(with: (any BrowserImportParsing).self)
    }
}
