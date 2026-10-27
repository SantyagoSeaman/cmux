public import Foundation

/// The parts cmux imports from a browser's export archive (Safari's Export Browsing Data, Google
/// Takeout's Chrome export): each Bookmarks.html, History.json and Passwords.csv, inflated by the
/// sandboxed service. A Passwords.csv crosses as bytes, as Chrome's utility importer sends
/// password forms to the browser process (chrome/common/importer/profile_import_process_param_traits);
/// the app moves it into `SecretBytes` and zeroes the copy.
public struct ExportArchiveParts: Sendable, Codable, Equatable {
    public var bookmarksHTML: [String]
    public var historyJSON: [Data]
    public var passwordCSVs: [Data]

    public init(bookmarksHTML: [String] = [], historyJSON: [Data] = [], passwordCSVs: [Data] = []) {
        self.bookmarksHTML = bookmarksHTML
        self.historyJSON = historyJSON
        self.passwordCSVs = passwordCSVs
    }

    public var isEmpty: Bool { bookmarksHTML.isEmpty && historyJSON.isEmpty && passwordCSVs.isEmpty }

    /// Larger than any browser's export.
    package static let maximumSize = 512 << 20
    /// One part, inflated.
    package static let maximumPart = 64 << 20

    /// Whether `head` (a file's first bytes) starts a zip archive.
    public static func isArchive(_ head: Data) -> Bool {
        head.starts(with: [0x50, 0x4B, 0x03, 0x04])
    }
}
