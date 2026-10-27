public import Foundation

/// A Firefox profile's session store file (mozLz4), found from folder listings only. The sandboxed
/// import service decompresses and parses it (`parse(_:)` in CmuxNextBrowserImportParsers,
/// plans/cmux-next/rule-of-2.md).
public struct FirefoxSessionReader {
    private let fileManager: FileManager

    /// Creates a reader with the filesystem used to discover Firefox sessions.
    ///
    /// - Parameter fileManager: Filesystem access for discovery.
    public init(fileManager: FileManager = FileManager()) {
        self.fileManager = fileManager
    }

    public func sessionFile(in profile: URL) -> URL? {
        let candidates = ["sessionstore-backups/recovery.jsonlz4", "sessionstore.jsonlz4", "sessionstore-backups/previous.jsonlz4"]
        return candidates.map { profile.appending(path: $0) }.first { fileManager.fileExists(atPath: $0.path) }
    }
}
