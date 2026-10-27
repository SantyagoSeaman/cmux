public import Foundation

/// Runs an ``ImportPlan``: for each source profile, reads each picked kind,
/// reports progress, then hands the batch to the destination. Runs off the
/// main thread; cancel the calling task to stop between reads (SQLite reads
/// also check every 256 rows). A profile that fails is reported and the
/// rest continue.
public actor BrowserImporter {
    private let provisioning: any BrowserProfileProvisioning
    private let store: ImportedDataStore?
    private let cookies: CookieImporter?
    private let passwords: PasswordImporter?
    private let reader: any ProfileDataReading

    /// `cookies` or `passwords` nil: that kind is skipped (no store to write to). `reader` parses the
    /// other kinds' files: the App's is the sandboxed import service (plans/cmux-next/rule-of-2.md).
    public init(provisioning: any BrowserProfileProvisioning = DefaultProfileOnly(), store: ImportedDataStore? = nil,
                cookies: CookieImporter? = nil, passwords: PasswordImporter? = nil, reader: any ProfileDataReading) {
        self.provisioning = provisioning
        self.store = store
        self.cookies = cookies
        self.passwords = passwords
        self.reader = reader
    }

    public func run(
        _ plan: ImportPlan,
        into destination: any ImportDestination,
        progress: @escaping @Sendable (ImportProgress) -> Void
    ) async throws -> ImportSummary {
        var batches: [ImportBatch] = []
        var failures: [String: String] = [:]
        var running = ImportCounts()
        let steps = max(1, plan.items.reduce(0) { $0 + $1.kinds.count + 1 })
        var done = 0
        for (index, item) in plan.items.enumerated() {
            try Task.checkCancellation()
            let report: @Sendable (ImportDataKind?, ImportCounts, Int) -> Void = { kind, counts, step in
                progress(ImportProgress(profileIndex: index, profileCount: plan.items.count, profile: item.profile,
                                        kind: kind, fraction: Double(step) / Double(steps), counts: counts))
            }
            do {
                var batch = ImportBatch(source: try await record(for: item.profile, target: plan.mergeTarget ?? item.target), kinds: item.kinds)
                if plan.keepsHistoryInSync, item.kinds.contains(.history) { batch.source.syncedHistoryFile = item.profile.historyFile }
                for kind in ImportDataKind.allCases where item.kinds.contains(kind) {
                    report(kind, running + batch.counts, done)
                    try Task.checkCancellation()
                    if kind == .cookies {
                        try await importCookies(item.profile, into: &batch)
                    } else if kind == .passwords {
                        try await importPasswords(item.profile, into: &batch)
                    } else {
                        try await read(kind, from: item.profile, historyLimit: plan.historyLimit, into: &batch)
                    }
                    done += 1
                }
                report(nil, running + batch.counts, done)
                try Task.checkCancellation()
                try await destination.commit(batch)
                done += 1
                running = running + batch.counts
                batches.append(batch)
            } catch is CancellationError {
                throw CancellationError()
            } catch {
                failures[item.profile.id] = String(describing: error)
                done += item.kinds.count + 1
            }
        }
        progress(ImportProgress(profileIndex: plan.items.count, profileCount: plan.items.count,
                                profile: plan.items.last?.profile ?? Self.placeholder, kind: nil, fraction: 1, counts: running))
        return ImportSummary(batches: batches, failures: failures)
    }

    /// Cookies fail on their own (a denied Keychain prompt must not lose the
    /// profile's bookmarks); the reason is kept in the batch.
    private func importCookies(_ profile: BrowserSourceProfile, into batch: inout ImportBatch) async throws {
        guard let cookies else { return }
        do {
            batch.cookies = try await cookies.run(profile, into: batch.source.targetProfileID)
        } catch let error as CookieImportError {
            batch.cookieError = error
        } catch is CancellationError {
            throw CancellationError()
        } catch {
            batch.cookieError = .malformed(String(describing: type(of: error)))
        }
    }

    /// Passwords fail on their own too, for the same reason.
    private func importPasswords(_ profile: BrowserSourceProfile, into batch: inout ImportBatch) async throws {
        guard let passwords else { return }
        do {
            batch.passwords = try await passwords.run(profile, intoProfile: batch.source.targetProfileID)
        } catch let error as PasswordImporter.Failure {
            batch.passwordError = error
        } catch is CancellationError {
            throw CancellationError()
        } catch {
            // A Login Data file that would not open: counted, never described (the message could hold a path or a row).
            batch.passwordError = .unreadable
        }
    }

    /// The mapping for a source: its proposed profile id (reused from an
    /// earlier import), and the profile the data goes to now: `target`, or
    /// the source's own profile, made with its name, color and picture.
    private func record(for profile: BrowserSourceProfile, target: String?) async throws -> ImportSourceRecord {
        let proposed = await store?.proposedProfileID(for: profile.id) ?? UUID().uuidString.lowercased()
        let single = profile.browser.family == .safari || profile.browser.family.isPrivateStore || profile.directoryName.isEmpty
        let name = single ? profile.browser.displayName : "\(profile.browser.displayName) · \(profile.displayName)"
        var record = ImportSourceRecord(browser: profile.browser, profileDirectory: profile.directoryName, displayName: name,
                                        proposedProfileID: proposed, targetProfileID: "default", profileName: single ? nil : profile.displayName)
        if let target {
            record.targetProfileID = target
        } else {
            record.targetProfileID = try await provisioning.createProfile(id: proposed, name: name, look: SourceProfileLook(profile),
                                                                          source: record.sourceFields)
        }
        return record
    }

    /// Reads one kind through `reader` (the sandboxed import service), except Safari's search engine,
    /// a Mac setting rather than a file.
    private func read(_ kind: ImportDataKind, from profile: BrowserSourceProfile, historyLimit: Int, into batch: inout ImportBatch) async throws {
        #if CMUX_NO_BROWSER_DATA_IMPORT || CMUX_NO_HISTORY_IMPORT
        // The cx-f58x notary test builds read no other browser's history, tabs, extensions or
        // bookmarks, and list none of their folders (CMUX_NO_HISTORY_IMPORT is the history-only
        // notary bisect build); the ProfileParseJob factories are compiled out the same way.
        return
        #else
        let path = profile.path
        switch (profile.browser.family, kind) {
        case (_, .bookmarks):
            let read = try await reader.bookmarks(profile)
            batch.bookmarks = read.items
            batch.skippedBookmarks = read.skipped
        case (_, .history):
            guard let file = profile.historyFile else { return }
            let read = try await reader.history(profile.browser.family, file: file, limit: historyLimit)
            batch.history = read.items
            batch.skippedHistory = read.skipped
        case (_, .openTabs):
            let read = try await reader.openTabs(profile)
            batch.openTabs = read.items
            batch.skippedOpenTabs = read.skipped
        case (.chromium, .searchEngines):
            batch.searchEngine = await ProfileSearchEngine(reader: reader).chromium(profile: path)
        case (.chromium, .extensions):
            batch.extensions = await ProfileExtensions(reader: reader).read(profile: path)
        case (.firefox, .searchEngines):
            batch.searchEngine = await ProfileSearchEngine(reader: reader).firefox(profile: path)
        case (.safari, .searchEngines):
            batch.searchEngine = SafariSearchEngineReader().read()
        default:
            return
        }
        #endif
    }

    private static let placeholder = BrowserSourceProfile(browser: .chrome, directoryName: "", displayName: "",
                                                          path: URL(fileURLWithPath: "/"), availability: [:])
}
