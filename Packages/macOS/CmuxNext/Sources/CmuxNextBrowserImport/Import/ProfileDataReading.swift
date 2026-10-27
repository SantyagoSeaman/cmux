public import Foundation

/// The untrusted step of an import: parses one of another browser's files into values. The
/// importer keeps the merge and every write, as Chrome splits its importer: the parse runs in the
/// utility process (chrome/utility/importer/profile_import_impl.cc) and the browser process takes
/// the values (chrome/browser/importer/external_process_importer_client.cc,
/// in_process_importer_bridge.cc). ``InProcessProfileReader`` parses here; the sandboxed service's
/// reader (CmuxNextBrowserImportService) parses in an XPC service.
public protocol ProfileDataReading: Sendable {
    func parse(_ job: ProfileParseJob) async throws -> ProfileParseResult
}

/// The parse in this process, on the browser's own file.
public struct InProcessProfileReader: ProfileDataReading {
    public init() {}

    public func parse(_ job: ProfileParseJob) async throws -> ProfileParseResult {
        try job.spec.parse(at: job.file)
    }
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
}
