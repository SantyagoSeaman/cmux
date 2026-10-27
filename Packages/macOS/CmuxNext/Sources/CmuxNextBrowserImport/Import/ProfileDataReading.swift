public import Foundation

/// The untrusted step of an import: parses one of another browser's files into values. The
/// importer keeps the merge, every key and every write, as Chrome splits its importer: the parse
/// runs in the utility process (chrome/utility/importer/profile_import_impl.cc) and the browser
/// process takes the values (chrome/browser/importer/external_process_importer_client.cc,
/// in_process_importer_bridge.cc). The App's reader is the sandboxed service's
/// (CmuxNextBrowserImportService); the readers are not linked into the App
/// (plans/cmux-next/rule-of-2.md).
public protocol ProfileDataReading: Sendable {
    /// A reader opens files: it does that off its caller's actor (a `@concurrent` step), so a
    /// main-actor caller never blocks on it.
    func parse(_ job: ProfileParseJob) async throws -> ProfileParseResult
}

/// A parse replied with another kind of value than its format reads: the reply is not trusted.
public struct ProfileParseMismatch: Error, Sendable, Equatable {
    public let format: ProfileParseSpec.Format

    public init(format: ProfileParseSpec.Format) {
        self.format = format
    }
}

extension ProfileDataReading {
    /// A profile's bookmarks; nothing for a source with no bookmark file to read.
    public func bookmarks(_ profile: BrowserSourceProfile) async throws -> ImportRead<ImportedBookmark> {
        guard let job = ProfileParseJob.bookmarks(profile) else { return ImportRead() }
        guard case .bookmarks(let read) = try await parse(job) else { throw ProfileParseMismatch(format: job.spec.format) }
        return read
    }

    /// One history database's pages, newest first; nothing for a family with no reader.
    public func history(_ family: BrowserFamily, file: URL, limit: Int) async throws -> ImportRead<ImportedHistoryEntry> {
        guard let job = ProfileParseJob.history(family, file: file, limit: limit) else { return ImportRead() }
        guard case .history(let read) = try await parse(job) else { throw ProfileParseMismatch(format: job.spec.format) }
        return read
    }

    /// The tabs of a profile's last session; nothing when it has no session file.
    public func openTabs(_ profile: BrowserSourceProfile) async throws -> ImportRead<ImportedTab> {
        guard let job = ProfileParseJob.openTabs(profile) else { return ImportRead() }
        guard case .openTabs(let read) = try await parse(job) else { throw ProfileParseMismatch(format: job.spec.format) }
        return read
    }

    /// A cookie store (`format` is one of the cookie formats), its Chromium values still sealed.
    public func cookies(_ format: ProfileParseSpec.Format, file: URL) async throws -> SealedCookies {
        let job = ProfileParseJob(format, file: file)
        guard case .cookies(let rows) = try await parse(job) else { throw ProfileParseMismatch(format: format) }
        return rows
    }

    /// One Chromium `Login Data` database's sign-ins, their passwords still sealed.
    public func chromiumLogins(_ file: URL) async throws -> SealedLogins {
        let job = ProfileParseJob(.chromiumLogins, file: file)
        guard case .chromiumLogins(let logins) = try await parse(job) else { throw ProfileParseMismatch(format: .chromiumLogins) }
        return logins
    }

    /// Firefox's `key4.db`, its master keys still sealed.
    public func firefoxKeyStore(_ file: URL) async throws -> FirefoxKeyStore {
        let job = ProfileParseJob(.firefoxKeyStore, file: file)
        guard case .firefoxKeyStore(let store) = try await parse(job) else { throw ProfileParseMismatch(format: .firefoxKeyStore) }
        return store
    }

    /// Firefox's `logins.json` entries, still sealed.
    public func firefoxLogins(_ file: URL) async throws -> [FirefoxLoginEntry] {
        let job = ProfileParseJob(.firefoxLogins, file: file)
        guard case .firefoxLogins(let entries) = try await parse(job) else { throw ProfileParseMismatch(format: .firefoxLogins) }
        return entries
    }

    /// A Chromium `Preferences` or `Secure Preferences` file.
    public func chromiumPreferences(_ file: URL) async throws -> ChromiumPreferences {
        let job = ProfileParseJob(.chromiumPreferences, file: file)
        guard case .preferences(let preferences) = try await parse(job) else { throw ProfileParseMismatch(format: .chromiumPreferences) }
        return preferences
    }

    /// A search engine file: Chromium's `Web Data` (the row with sync GUID `key`) or Firefox's `search.json.mozlz4`.
    public func searchEngine(_ format: ProfileParseSpec.Format, file: URL, key: String = "") async throws -> ImportedSearchEngine? {
        let job = ProfileParseJob(format, file: file, key: key)
        guard case .searchEngine(let engine) = try await parse(job) else { throw ProfileParseMismatch(format: format) }
        return engine
    }

    /// One version folder's `manifest.json`; nil when it is not a JSON object.
    public func extensionManifest(_ file: URL) async throws -> ChromiumExtensionManifest? {
        let job = ProfileParseJob(.chromiumExtensionManifest, file: file)
        guard case .extensionManifest(let manifest) = try await parse(job) else {
            throw ProfileParseMismatch(format: .chromiumExtensionManifest)
        }
        return manifest
    }

    /// The message named `key` (lowercased) in a locale's `messages.json`; nil when it has none.
    public func extensionMessage(_ file: URL, key: String) async throws -> String? {
        let job = ProfileParseJob(.chromiumExtensionMessage, file: file, key: key)
        guard case .extensionMessage(let message) = try await parse(job) else { throw ProfileParseMismatch(format: .chromiumExtensionMessage) }
        return message
    }

    /// The parts of an export archive the person picked.
    public func exportArchive(_ file: URL) async throws -> ExportArchiveParts {
        let job = ProfileParseJob(.exportArchive, file: file)
        guard case .exportArchive(let parts) = try await parse(job) else { throw ProfileParseMismatch(format: .exportArchive) }
        return parts
    }
}
