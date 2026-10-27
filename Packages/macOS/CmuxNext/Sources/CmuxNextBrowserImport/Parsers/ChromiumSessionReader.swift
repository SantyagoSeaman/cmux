public import Foundation

/// A Chromium profile's session files (`Sessions/Session_<n>`, or `Current Session` on older
/// versions), found from folder listings only. The sandboxed import service parses them
/// (`parse(_:)` in CmuxNextBrowserImportParsers, plans/cmux-next/rule-of-2.md).
public struct ChromiumSessionReader {
    private let fileManager: FileManager

    /// Creates a reader with the filesystem used to discover Chromium sessions.
    ///
    /// - Parameter fileManager: Filesystem access for discovery.
    public init(fileManager: FileManager = FileManager()) {
        self.fileManager = fileManager
    }

    /// The newest `Session_*` file in a profile's `Sessions` folder.
    public func latestSessionFile(in sessions: URL) -> URL? {
        let names = (try? fileManager.contentsOfDirectory(atPath: sessions.path)) ?? []
        return names.filter { $0.hasPrefix("Session_") }
            .max { (Int64($0.dropFirst(8)) ?? 0) < (Int64($1.dropFirst(8)) ?? 0) }
            .map { sessions.appending(path: $0) }
    }

    public func sessionFile(profile: URL) -> URL? {
        if let latest = latestSessionFile(in: profile.appending(path: "Sessions")) { return latest }
        let legacy = profile.appending(path: "Current Session")
        return fileManager.fileExists(atPath: legacy.path) ? legacy : nil
    }
}
