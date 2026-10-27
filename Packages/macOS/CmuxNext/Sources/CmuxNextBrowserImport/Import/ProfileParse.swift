public import Foundation

/// One parse of another browser's file: which reader runs, with what. It names no file, so it can
/// cross to the sandboxed import service, which gets the bytes the app opened and never a path
/// (plans/cmux-next/rule-of-2.md). The readers themselves are linked only into the service
/// (CmuxNextBrowserImportParsers); this module keeps the jobs, the values and the merge.
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
        case chromiumCookies
        case firefoxCookies
        case safariCookies
        case chromiumLogins
        case firefoxKeyStore
        case firefoxLogins
        /// `Preferences` or `Secure Preferences`.
        case chromiumPreferences
        /// `Web Data`'s `keywords` row whose sync GUID is ``ProfileParseSpec/key``.
        case chromiumSearchEngine
        case firefoxSearchEngine
        /// One version folder's `manifest.json`.
        case chromiumExtensionManifest
        /// One locale's `messages.json`: the message named ``ProfileParseSpec/key``.
        case chromiumExtensionMessage
        case exportArchive

        /// A SQLite database: its `-wal` and `-journal` go with it (as SQLiteSnapshot copies them).
        public var isDatabase: Bool {
            switch self {
            case .firefoxBookmarks, .chromiumHistory, .firefoxHistory, .safariHistory, .chromiumCookies, .firefoxCookies, .chromiumLogins,
                 .firefoxKeyStore, .chromiumSearchEngine:
                true
            case .chromiumBookmarks, .safariBookmarks, .arcSidebar, .chromiumSession, .firefoxSession, .safariCookies, .firefoxLogins,
                 .chromiumPreferences, .firefoxSearchEngine, .chromiumExtensionManifest, .chromiumExtensionMessage, .exportArchive:
                false
            }
        }

        /// The kind of value this format's reader gives.
        public var kind: ProfileParseResult.Kind {
            switch self {
            case .chromiumBookmarks, .safariBookmarks, .arcSidebar, .firefoxBookmarks: .bookmarks
            case .chromiumHistory, .firefoxHistory, .safariHistory: .history
            case .chromiumSession, .firefoxSession: .openTabs
            case .chromiumCookies, .firefoxCookies, .safariCookies: .cookies
            case .chromiumLogins: .chromiumLogins
            case .firefoxKeyStore: .firefoxKeyStore
            case .firefoxLogins: .firefoxLogins
            case .chromiumPreferences: .preferences
            case .chromiumSearchEngine, .firefoxSearchEngine: .searchEngine
            case .chromiumExtensionManifest: .extensionManifest
            case .chromiumExtensionMessage: .extensionMessage
            case .exportArchive: .exportArchive
            }
        }
    }

    public var format: Format
    /// Arc's sidebar only: the profile whose spaces are read.
    public var profileDirectory: String
    /// History only: how many of the newest pages are read.
    public var historyLimit: Int
    /// `chromiumSearchEngine`: the engine's sync GUID; `chromiumExtensionMessage`: the message name, lowercased.
    public var key: String

    public init(format: Format, profileDirectory: String = "", historyLimit: Int = ImportPlan.defaultHistoryLimit, key: String = "") {
        self.format = format
        self.profileDirectory = profileDirectory
        self.historyLimit = historyLimit
        self.key = key
    }
}

/// What one parse read.
public enum ProfileParseResult: Sendable, Codable, Equatable {
    case bookmarks(ImportRead<ImportedBookmark>)
    case history(ImportRead<ImportedHistoryEntry>)
    case openTabs(ImportRead<ImportedTab>)
    case cookies(SealedCookies)
    case chromiumLogins(SealedLogins)
    case firefoxKeyStore(FirefoxKeyStore)
    case firefoxLogins([FirefoxLoginEntry])
    case preferences(ChromiumPreferences)
    case searchEngine(ImportedSearchEngine?)
    case extensionManifest(ChromiumExtensionManifest?)
    case extensionMessage(String?)
    case exportArchive(ExportArchiveParts)

    /// A result's case, without its value.
    public enum Kind: Sendable, Equatable {
        case bookmarks, history, openTabs, cookies, chromiumLogins, firefoxKeyStore, firefoxLogins, preferences, searchEngine
        case extensionManifest, extensionMessage, exportArchive
    }

    public var kind: Kind {
        switch self {
        case .bookmarks: .bookmarks
        case .history: .history
        case .openTabs: .openTabs
        case .cookies: .cookies
        case .chromiumLogins: .chromiumLogins
        case .firefoxKeyStore: .firefoxKeyStore
        case .firefoxLogins: .firefoxLogins
        case .preferences: .preferences
        case .searchEngine: .searchEngine
        case .extensionManifest: .extensionManifest
        case .extensionMessage: .extensionMessage
        case .exportArchive: .exportArchive
        }
    }
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

    public init(_ format: ProfileParseSpec.Format, file: URL, key: String = "") {
        self.init(spec: ProfileParseSpec(format: format, key: key), file: file)
    }

    /// A profile's bookmarks, with the reader its registry row names (engine family, not browser);
    /// nil for a private store or an HTML export, which have nothing to read here, and in the
    /// cx-f58x notary builds, which touch no other browser's files.
    public static func bookmarks(_ profile: BrowserSourceProfile) -> ProfileParseJob? {
        #if CMUX_NO_BROWSER_DATA_IMPORT || CMUX_NO_HISTORY_IMPORT
        return nil
        #else
        let files = profile.browser.source?.files
        func file(_ fallback: String) -> URL { profile.bookmarksFile ?? profile.path.appending(path: files?.bookmarks ?? fallback) }
        switch profile.browser.bookmarksFormat {
        case .chromiumJSON:
            return ProfileParseJob(.chromiumBookmarks, file: file("Bookmarks"))
        case .firefoxPlaces:
            return ProfileParseJob(.firefoxBookmarks, file: file("places.sqlite"))
        case .safariPlist:
            return ProfileParseJob(.safariBookmarks, file: file("Bookmarks.plist"))
        case .arcSidebar:
            // No sidebar file: Arc's Chromium `Bookmarks` file is the fallback.
            guard let sidebar = profile.bookmarksFile else {
                return ProfileParseJob(.chromiumBookmarks, file: profile.path.appending(path: files?.bookmarks ?? "Bookmarks"))
            }
            return ProfileParseJob(spec: ProfileParseSpec(format: .arcSidebar, profileDirectory: profile.directoryName), file: sidebar)
        case .htmlExport, .noStore:
            return nil
        }
        #endif
    }

    /// One history database, newest pages first; nil for a family with no reader, and in the
    /// notary builds.
    public static func history(_ family: BrowserFamily, file: URL, limit: Int) -> ProfileParseJob? {
        #if CMUX_NO_BROWSER_DATA_IMPORT || CMUX_NO_HISTORY_IMPORT
        return nil
        #else
        let format: ProfileParseSpec.Format
        switch family {
        case .chromium: format = .chromiumHistory
        case .firefox: format = .firefoxHistory
        case .safari: format = .safariHistory
        case .webkit, .other: return nil
        }
        return ProfileParseJob(spec: ProfileParseSpec(format: format, historyLimit: limit), file: file)
        #endif
    }

    /// The tabs of a profile's last session; nil when it has no session file, its family no
    /// reader, and in the notary builds (which do not list the session folders either).
    public static func openTabs(_ profile: BrowserSourceProfile) -> ProfileParseJob? {
        #if CMUX_NO_BROWSER_DATA_IMPORT || CMUX_NO_HISTORY_IMPORT
        return nil
        #else
        switch profile.browser.family {
        case .chromium:
            return ChromiumSessionReader().sessionFile(profile: profile.path).map { ProfileParseJob(.chromiumSession, file: $0) }
        case .firefox:
            return FirefoxSessionReader().sessionFile(in: profile.path).map { ProfileParseJob(.firefoxSession, file: $0) }
        case .safari, .webkit, .other:
            return nil
        }
        #endif
    }
}
