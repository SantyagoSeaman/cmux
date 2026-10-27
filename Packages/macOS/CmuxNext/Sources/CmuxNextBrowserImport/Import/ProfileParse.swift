import CmuxNextCompat
public import Foundation

/// One parse of another browser's file: which reader runs, with what. It names no file, so it can
/// cross to the sandboxed import service (CmuxNextBrowserImportService), which gets the bytes the
/// app opened and never a path (plans/cmux-next/rule-of-2.md). ``parse(at:)`` is the one parse:
/// in this process (``InProcessProfileReader``) on the browser's file, in the service on its staged
/// copy, so both give the same result.
public struct ProfileParseSpec: Sendable, Codable, Equatable {
    /// The reader, by the file it reads.
    public enum Format: String, Sendable, Codable, CaseIterable {
        case chromiumBookmarks
        case safariBookmarks
        case arcSidebar
        case firefoxBookmarks
        case chromiumHistory
        case firefoxHistory
        case safariHistory
        case chromiumSession
        case firefoxSession

        /// A SQLite database: its `-wal` and `-journal` go with it (``SQLiteSnapshot`` reads them).
        public var isDatabase: Bool {
            switch self {
            case .firefoxBookmarks, .chromiumHistory, .firefoxHistory, .safariHistory: true
            case .chromiumBookmarks, .safariBookmarks, .arcSidebar, .chromiumSession, .firefoxSession: false
            }
        }
    }

    public var format: Format
    /// Arc's sidebar only: the profile whose spaces are read.
    public var profileDirectory: String
    /// History only: how many of the newest pages are read.
    public var historyLimit: Int

    public init(format: Format, profileDirectory: String = "", historyLimit: Int = ImportPlan.defaultHistoryLimit) {
        self.format = format
        self.profileDirectory = profileDirectory
        self.historyLimit = historyLimit
    }

    /// Runs the format's reader on `file` (a database's side files beside it, as `<file>-wal`).
    public func parse(at file: URL) throws -> ProfileParseResult {
        #if CMUX_NO_BROWSER_DATA_IMPORT || CMUX_NO_HISTORY_IMPORT
        // The cx-f58x notary test builds read no other browser's history, tabs or bookmarks.
        switch format {
        case .chromiumBookmarks, .safariBookmarks, .arcSidebar, .firefoxBookmarks: return .bookmarks(ImportRead())
        case .chromiumHistory, .firefoxHistory, .safariHistory: return .history(ImportRead())
        case .chromiumSession, .firefoxSession: return .openTabs(ImportRead())
        }
        #else
        switch format {
        case .chromiumBookmarks: return .bookmarks(try ChromiumBookmarksParser().parse(Data(blockingContentsOf: file)))
        case .safariBookmarks: return .bookmarks(try SafariBookmarksParser().parse(Data(blockingContentsOf: file)))
        case .arcSidebar: return .bookmarks(try ArcSidebarReader().parse(Data(blockingContentsOf: file), profileDirectory: profileDirectory))
        case .firefoxBookmarks: return .bookmarks(try FirefoxPlacesReader().readBookmarks(file))
        case .chromiumHistory: return .history(try ChromiumHistoryReader().read(file, limit: historyLimit))
        case .firefoxHistory: return .history(try FirefoxPlacesReader().readHistory(file, limit: historyLimit))
        case .safariHistory: return .history(try SafariHistoryReader().read(file, limit: historyLimit))
        case .chromiumSession: return .openTabs(ChromiumSessionReader().parse(try Data(blockingContentsOf: file)))
        case .firefoxSession: return .openTabs(FirefoxSessionReader().parse(try Data(blockingContentsOf: file)))
        }
        #endif
    }
}

/// What one parse read.
public enum ProfileParseResult: Sendable, Codable, Equatable {
    case bookmarks(ImportRead<ImportedBookmark>)
    case history(ImportRead<ImportedHistoryEntry>)
    case openTabs(ImportRead<ImportedTab>)
}

/// A parse and the file it reads. The app makes it from the registry and folder listings only
/// (which reader, which file); nothing here opens the file.
public struct ProfileParseJob: Sendable, Equatable {
    public var spec: ProfileParseSpec
    public var file: URL

    public init(spec: ProfileParseSpec, file: URL) {
        self.spec = spec
        self.file = file
    }

    /// A profile's bookmarks, with the reader its registry row names (engine family, not browser);
    /// nil for a private store or an HTML export, which have nothing to read here.
    public static func bookmarks(_ profile: BrowserSourceProfile) -> ProfileParseJob? {
        let files = profile.browser.source?.files
        func file(_ fallback: String) -> URL { profile.bookmarksFile ?? profile.path.appending(path: files?.bookmarks ?? fallback) }
        switch profile.browser.bookmarksFormat {
        case .chromiumJSON:
            return ProfileParseJob(spec: ProfileParseSpec(format: .chromiumBookmarks), file: file("Bookmarks"))
        case .firefoxPlaces:
            return ProfileParseJob(spec: ProfileParseSpec(format: .firefoxBookmarks), file: file("places.sqlite"))
        case .safariPlist:
            return ProfileParseJob(spec: ProfileParseSpec(format: .safariBookmarks), file: file("Bookmarks.plist"))
        case .arcSidebar:
            // No sidebar file: Arc's Chromium `Bookmarks` file is the fallback.
            guard let sidebar = profile.bookmarksFile else {
                return ProfileParseJob(spec: ProfileParseSpec(format: .chromiumBookmarks),
                                       file: profile.path.appending(path: files?.bookmarks ?? "Bookmarks"))
            }
            return ProfileParseJob(spec: ProfileParseSpec(format: .arcSidebar, profileDirectory: profile.directoryName), file: sidebar)
        case .htmlExport, .noStore:
            return nil
        }
    }

    /// One history database, newest pages first; nil for a family with no reader.
    public static func history(_ family: BrowserFamily, file: URL, limit: Int) -> ProfileParseJob? {
        let format: ProfileParseSpec.Format
        switch family {
        case .chromium: format = .chromiumHistory
        case .firefox: format = .firefoxHistory
        case .safari: format = .safariHistory
        case .webkit, .other: return nil
        }
        return ProfileParseJob(spec: ProfileParseSpec(format: format, historyLimit: limit), file: file)
    }

    /// The tabs of a profile's last session; nil when it has no session file, or its family no reader.
    public static func openTabs(_ profile: BrowserSourceProfile) -> ProfileParseJob? {
        switch profile.browser.family {
        case .chromium:
            return ChromiumSessionReader().sessionFile(profile: profile.path)
                .map { ProfileParseJob(spec: ProfileParseSpec(format: .chromiumSession), file: $0) }
        case .firefox:
            return FirefoxSessionReader().sessionFile(in: profile.path)
                .map { ProfileParseJob(spec: ProfileParseSpec(format: .firefoxSession), file: $0) }
        case .safari, .webkit, .other:
            return nil
        }
    }
}
