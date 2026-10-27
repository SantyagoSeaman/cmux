public import CmuxNextBrowserImport
import CmuxNextCompat
public import Foundation

extension ProfileParseSpec {
    /// Runs the format's reader on `file` (a database's side files beside it, as `<file>-wal`): the
    /// one parse, in the sandboxed service on its staged copy, and in tests through
    /// ``InProcessProfileReader``.
    public func parse(at file: URL) throws -> ProfileParseResult {
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
        case .chromiumCookies: return .cookies(try ChromiumCookieReader().read(file))
        case .firefoxCookies: return .cookies(try FirefoxCookieReader().read(file))
        case .safariCookies: return .cookies(try SafariBinaryCookies().parse(Data(blockingContentsOf: file)))
        case .chromiumLogins, .firefoxKeyStore, .firefoxLogins: return try parsePasswords(at: file)
        case .chromiumPreferences: return .preferences(try ChromiumSearchEngineReader().preferences(Data(blockingContentsOf: file)))
        case .chromiumSearchEngine: return .searchEngine(try ChromiumSearchEngineReader().engine(webData: file, guid: key))
        case .firefoxSearchEngine: return .searchEngine(FirefoxSearchEngineReader().read(try Data(blockingContentsOf: file)))
        case .chromiumExtensionManifest: return .extensionManifest(ChromiumExtensionsReader().manifest(try Data(blockingContentsOf: file)))
        case .chromiumExtensionMessage:
            return .extensionMessage(ChromiumExtensionsReader().message(try Data(blockingContentsOf: file), key: key))
        case .exportArchive: return .exportArchive(try ExportArchive(contentsOf: file).parts)
        }
    }

    private func parsePasswords(at file: URL) throws -> ProfileParseResult {
        #if CMUX_NO_PASSWORD_IMPORT
        // The cx-f58x notary test build has no browser password readers.
        throw PasswordImporter.Failure.unsupportedBrowser
        #else
        switch format {
        case .chromiumLogins: return .chromiumLogins(try ChromiumLoginRows().read(file))
        case .firefoxKeyStore: return .firefoxKeyStore(try FirefoxKeyStoreReader().read(file))
        default: return .firefoxLogins(try FirefoxLoginsFile().parse(Data(blockingContentsOf: file)))
        }
        #endif
    }
}

/// The parse in this process, on the browser's own file: the same parse the service runs, for
/// tests that compare the two and for the fuzz of task 025. The App never links it.
public struct InProcessProfileReader: ProfileDataReading {
    public init() {}

    @concurrent public func parse(_ job: ProfileParseJob) async throws -> ProfileParseResult {
        try job.spec.parse(at: job.file)
    }
}
