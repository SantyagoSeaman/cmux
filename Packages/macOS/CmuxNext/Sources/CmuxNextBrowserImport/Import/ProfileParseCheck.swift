import Foundation

extension ProfileParseResult {
    /// This result as the App takes it from the sandboxed import service, which may be compromised
    /// (plans/cmux-next/rule-of-2.md): the decode built only the wire's types, and this applies the
    /// readers' invariants the decode skips, as a Mojo typemap's `StructTraits::Read` checks a
    /// child's message in the browser process (mojo/public/cpp/bindings/struct_traits.h). Nil
    /// refuses the reply: another kind than `spec` reads, a counter that is negative or out of
    /// range, more entries than a reader gives. An entry a reader would not have given (an address
    /// that is not importable, a time out of range) is dropped and counted, as the readers count it.
    /// A reply from a reader that kept its invariants comes back unchanged.
    package func checked(for spec: ProfileParseSpec) -> ProfileParseResult? {
        guard kind == spec.format.kind else { return nil }
        switch self {
        case .bookmarks(let read):
            guard ImportReplyCheck.counts(read.skipped), read.items.count <= ImportReplyCheck.maximumBookmarks else { return nil }
            return .bookmarks(ImportReplyCheck.bookmarks(read))
        case .history(let read):
            guard ImportReplyCheck.counts(read.skipped), read.items.count <= ImportReplyCheck.maximumEntries else { return nil }
            return .history(ImportReplyCheck.history(read, limit: spec.historyLimit))
        case .openTabs(let read):
            guard ImportReplyCheck.counts(read.skipped), read.items.count <= ImportReplyCheck.maximumTabs else { return nil }
            return .openTabs(ImportReplyCheck.tabs(read))
        case .cookies(let rows):
            return ImportReplyCheck.cookies(rows).map(ProfileParseResult.cookies)
        case .chromiumLogins(let logins):
            return ImportReplyCheck.logins(logins).map(ProfileParseResult.chromiumLogins)
        case .firefoxKeyStore(let store):
            return store.keys.count <= ImportReplyCheck.maximumKeys ? self : nil
        case .firefoxLogins(let entries):
            return entries.count <= ImportReplyCheck.maximumEntries ? self : nil
        case .preferences(let preferences):
            guard preferences.extensions.count <= ImportReplyCheck.maximumKeys else { return nil }
            var checked = preferences
            checked.searchEngine = preferences.searchEngine.flatMap(ImportReplyCheck.engine)
            return .preferences(checked)
        case .searchEngine(let engine):
            return .searchEngine(engine.flatMap(ImportReplyCheck.engine))
        case .extensionManifest:
            return self
        case .extensionMessage(let message):
            return .extensionMessage(message.flatMap { $0.utf8.count <= ImportReplyCheck.maximumText ? $0 : nil })
        case .exportArchive(let parts):
            return ImportReplyCheck.archive(parts) ? self : nil
        }
    }
}

/// The checks of one reply. Every bound is past what a reader gives, so an honest reply passes as it is.
struct ImportReplyCheck {
    /// A counter in a reply is at most this (no reader counts more), so the App's sums of every
    /// profile's counters stay far from `Int.max`.
    static let maximumCount = 2_147_483_647
    /// Bookmarks of one profile: Safari's reader stops at 2^18 entries; more than this is no real profile.
    static let maximumBookmarks = 1 << 22
    /// Open tabs of one session.
    static let maximumTabs = 1 << 20
    /// History pages, cookies of one store, sign-ins of one database, `logins.json` entries.
    static let maximumEntries = 1 << 22
    /// Master keys of one key store, extensions of one `Preferences` file.
    static let maximumKeys = 1 << 16
    /// A folder path's names: a reader opens folders `ImportSkipCounts.maximumDepth` deep at most.
    static let maximumPath = 2 * ImportSkipCounts.maximumDepth
    /// An extension's display name, in UTF-8 bytes.
    static let maximumText = 4096
    /// Parts of one export archive.
    static let maximumParts = 1024

    /// Whether every counter is in `0...maximumCount`.
    static func counts(_ skipped: ImportSkipCounts) -> Bool {
        [skipped.notImportable, skipped.badTime, skipped.repeatedFolder, skipped.tooDeep, skipped.overLimit, skipped.unsupported]
            .allSatisfy { (0...maximumCount).contains($0) }
    }

    static func counts(_ skipped: LoginSkipCounts) -> Bool {
        [skipped.neverSaved, skipped.notWebForm, skipped.empty, skipped.undecryptable, skipped.duplicate]
            .allSatisfy { (0...maximumCount).contains($0) }
    }

    /// The address as a reader gives it (parsed again from its text), or nil when no reader would.
    private static func importable(_ url: URL) -> URL? {
        ImportableURL(url.absoluteString)?.url
    }

    /// The bookmarks a reader could give; the others counted (the counts are bounded, so the sum fits).
    static func bookmarks(_ read: ImportRead<ImportedBookmark>) -> ImportRead<ImportedBookmark> {
        var checked = ImportRead<ImportedBookmark>(skipped: read.skipped)
        for item in read.items {
            guard let url = importable(item.url) else {
                checked.skipped.notImportable += 1
                continue
            }
            guard item.folderPath.count <= maximumPath else {
                checked.skipped.tooDeep += 1
                continue
            }
            var bookmark = item
            bookmark.url = url
            bookmark.dateAdded = item.dateAdded.flatMap(BrowserTime.kept)
            checked.items.append(bookmark)
        }
        return checked
    }

    /// The newest `limit` pages at most (the readers' `LIMIT`; pages past it are not counted).
    static func history(_ read: ImportRead<ImportedHistoryEntry>, limit: Int) -> ImportRead<ImportedHistoryEntry> {
        var checked = ImportRead<ImportedHistoryEntry>(skipped: read.skipped)
        for item in read.items.prefix(max(0, limit)) {
            guard let url = importable(item.url) else {
                checked.skipped.notImportable += 1
                continue
            }
            guard let lastVisit = BrowserTime.kept(item.lastVisit) else {
                checked.skipped.badTime += 1
                continue
            }
            // The initializer clamps the visits as a reader's does.
            checked.items.append(ImportedHistoryEntry(url: url, title: item.title, visitCount: item.visitCount, lastVisit: lastVisit))
        }
        return checked
    }

    static func tabs(_ read: ImportRead<ImportedTab>) -> ImportRead<ImportedTab> {
        var checked = ImportRead<ImportedTab>(skipped: read.skipped)
        for item in read.items {
            guard let url = importable(item.url) else {
                checked.skipped.notImportable += 1
                continue
            }
            guard (0...maximumTabs).contains(item.window) else {
                checked.skipped.unsupported += 1
                continue
            }
            checked.items.append(ImportedTab(url: url, title: item.title, window: item.window, pinned: item.pinned))
        }
        return checked
    }

    /// The rows again through the cookie's initializer (its path rule), with every time checked.
    static func cookies(_ rows: SealedCookies) -> SealedCookies? {
        guard (0...maximumCount).contains(rows.partitioned), rows.cookies.count <= maximumEntries else { return nil }
        let cookies = rows.cookies.map { row in
            let cookie = row.cookie
            return SealedCookie(cookie: ImportedCookie(name: cookie.name, value: cookie.value, domain: cookie.domain, path: cookie.path,
                                                       secure: cookie.secure, httpOnly: cookie.httpOnly, sameSite: cookie.sameSite,
                                                       expires: cookie.expires.flatMap(BrowserTime.keptExpiry),
                                                       created: cookie.created.flatMap(BrowserTime.kept),
                                                       lastAccess: cookie.lastAccess.flatMap(BrowserTime.kept)),
                                sealedValue: row.sealedValue)
        }
        return SealedCookies(cookies: cookies, databaseVersion: rows.databaseVersion, partitioned: rows.partitioned)
    }

    static func logins(_ logins: SealedLogins) -> SealedLogins? {
        guard counts(logins.skipped), logins.logins.count <= maximumEntries else { return nil }
        var checked = logins
        checked.logins = logins.logins.map { login in
            var login = login
            login.created = login.created.flatMap(BrowserTime.kept)
            return login
        }
        return checked
    }

    /// An engine as the readers keep it: an https search address that takes the typed text, and a
    /// suggest address only when it does too.
    static func engine(_ engine: ImportedSearchEngine) -> ImportedSearchEngine? {
        guard ImportedSearchEngine.takesTypedText(engine.search) else { return nil }
        var checked = engine
        checked.suggest = engine.suggest.flatMap { ImportedSearchEngine.takesTypedText($0) ? $0 : nil }
        return checked
    }

    /// Whether every part fits the archive reader's caps (a bookmarks file's text may hold up to
    /// three bytes for each byte it replaced as not UTF-8).
    static func archive(_ parts: ExportArchiveParts) -> Bool {
        let part = ExportArchiveParts.maximumPart
        return parts.bookmarksHTML.count + parts.historyJSON.count <= maximumParts && parts.passwordCSVs.count <= 1
            && parts.bookmarksHTML.allSatisfy { $0.utf8.count <= 3 * part }
            && parts.historyJSON.allSatisfy { $0.count <= part } && parts.passwordCSVs.allSatisfy { $0.count <= part }
    }
}
