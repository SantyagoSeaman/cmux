/// Entries of a source's bookmarks, history or open tabs that a reader left out, by reason, so a
/// damaged profile reports what it lost (cookies and passwords count theirs in ``CookieReadResult``
/// and ``LoginSkipCounts``). Entries that are by design not data (Firefox tag assignments, hidden
/// Chromium pages, Safari's History proxy, rows past the history limit) are not counted.
public struct ImportSkipCounts: Sendable, Codable, Equatable {
    /// No address, or one that is not a web page or a file (``ImportableURL``): `chrome://`, `about:`.
    public var notImportable = 0
    /// A visit time that is missing or outside years 1-9999 (``BrowserTime``).
    public var badTime = 0
    /// A folder the file names again, inside itself or from a second parent: it is read once.
    public var repeatedFolder = 0
    /// A folder deeper than ``maximumDepth``.
    public var tooDeep = 0
    /// Entries past a reader's cap on how many it reads (a folder not opened counts once).
    public var overLimit = 0
    /// An entry the reader does not read: an unknown kind, a dangling id, a tab with no page.
    public var unsupported = 0

    /// Folders deeper than this are not opened.
    public static let maximumDepth = 64

    public init() {}

    /// Every counter added up; a sum past `Int.max` stays at `Int.max` (counts are reports, never indices).
    public var total: Int {
        [badTime, repeatedFolder, tooDeep, overLimit, unsupported].reduce(notImportable, Self.saturating)
    }

    public static func + (lhs: ImportSkipCounts, rhs: ImportSkipCounts) -> ImportSkipCounts {
        var sum = lhs
        sum.notImportable = saturating(lhs.notImportable, rhs.notImportable)
        sum.badTime = saturating(lhs.badTime, rhs.badTime)
        sum.repeatedFolder = saturating(lhs.repeatedFolder, rhs.repeatedFolder)
        sum.tooDeep = saturating(lhs.tooDeep, rhs.tooDeep)
        sum.overLimit = saturating(lhs.overLimit, rhs.overLimit)
        sum.unsupported = saturating(lhs.unsupported, rhs.unsupported)
        return sum
    }

    private static func saturating(_ lhs: Int, _ rhs: Int) -> Int {
        let (sum, overflow) = lhs.addingReportingOverflow(rhs)
        return overflow ? (rhs > 0 ? .max : .min) : sum
    }
}

/// What a reader got from one file: the entries, and the ones it left out by reason.
public struct ImportRead<Item: Sendable>: Sendable {
    public var items: [Item]
    public var skipped: ImportSkipCounts

    public init(items: [Item] = [], skipped: ImportSkipCounts = ImportSkipCounts()) {
        self.items = items
        self.skipped = skipped
    }
}

/// A read crosses the sandboxed import service's wire (CmuxNextBrowserImportService) as a value.
extension ImportRead: Codable where Item: Codable {}

extension ImportRead: Equatable where Item: Equatable {}
