import CmuxFuzzSupport
@testable import CmuxNextBrowserImport
@testable import CmuxNextBrowserImportParsers
@testable import CmuxNextBrowserImportService
import Foundation
import Testing

/// The sandboxed import service (p10-stability-208, plans/cmux-next/rule-of-2.md) over a real XPC
/// connection: an anonymous listener in the test process exports the same ``BrowserImportServiceGate``
/// the service's main runs, so the files cross as descriptors and the values as the wire's bytes.
/// The import must read the same through it as in process, a hostile reply must be refused or
/// cleaned, never trap the App, and the fuzz of task 025 runs against it. Gated like the fuzz:
/// agents run it on the fleet, CI does not (Lawrence 2026-10-23):
///
/// ```
/// CMUX_IMPORT_FUZZ=1 swift test --filter BrowserImportServiceTests
/// ```
@Suite(
    .serialized,
    .enabled(
        if: ProcessInfo.processInfo.environment["CMUX_IMPORT_FUZZ"] == "1",
        "set CMUX_IMPORT_FUZZ=1; agents run it on the fleet (Lawrence 2026-10-23)"
    )
)
struct BrowserImportServiceTests {
    /// Every format the service reads gives the same value through it as in process.
    @Test(.timeLimit(.minutes(1)))
    func everyFormatReadsTheSameThroughTheService() async throws {
        let home = try ImportServiceScratch()
        defer { home.remove() }
        let profiles = try ImportFuzzHome(home.folder)
        var jobs = try ImportServiceJobs.extra(in: home.folder)
        for profile in profiles.profiles {
            jobs += [ProfileParseJob.bookmarks(profile.source), ProfileParseJob.openTabs(profile.source)].compactMap { $0 }
            if let file = profile.source.historyFile,
               let job = ProfileParseJob.history(profile.source.browser.family, file: file, limit: ImportPlan.defaultHistoryLimit) {
                jobs.append(job)
            }
            jobs += ImportServiceJobs.secrets(profile.source)
        }
        #if CMUX_NO_PASSWORD_IMPORT
        let formats = Set(ProfileParseSpec.Format.allCases).subtracting([.chromiumLogins, .firefoxKeyStore, .firefoxLogins])
        #else
        let formats = Set(ProfileParseSpec.Format.allCases)
        #endif
        #expect(Set(jobs.map(\.spec.format)) == formats)
        let service = BrowserImportServiceReader(connector: AnonymousImportService())
        for job in jobs {
            let local = try await InProcessProfileReader().parse(job)
            let remote = try await service.parse(job)
            #expect(remote == local, "\(job.spec.format.rawValue) read differently through the service")
        }
    }

    /// Whole profiles with damaged files, as ``BrowserImportFuzzTests/damagedProfilesAreReportedOneByOne()``,
    /// parsed by the service (cookies and passwords too): each profile is reported once, an undamaged
    /// one imports in full.
    @Test(.timeLimit(.minutes(BrowserImportFuzzTests.timeLimitMinutes)))
    func damagedProfilesAreReportedOneByOneThroughTheService() async throws {
        let run = try FuzzRun("BrowserImport.service.profiles", seed: 0x1A90_0208_0001, iterations: 100)
        let reader = BrowserImportServiceReader(connector: AnonymousImportService())
        try await ImportFuzzThread.run("BrowserImportFuzz service profiles") {
            _ = try run.forEach { rng in
                try ImportFuzzFiles.scratch("service-home") { (home: URL) -> Void in
                    var profiles = try ImportFuzzHome(home)
                    try profiles.damage(&rng)
                    let checked = profiles
                    try ImportFuzzThread.wait { try await checked.check(reader: reader) }
                }
            }
        }
    }

    /// A compromised service's well-formed reply: counters that would overflow the App's sums, or are
    /// negative, are refused; addresses no reader gives (`javascript:`, `chrome:`) are dropped and
    /// counted; visit counts are clamped; pages past the history limit and times out of range go.
    @Test func aHostileReplyIsRefusedOrCleanedNeverTrusted() async throws {
        let home = try ImportServiceScratch()
        defer { home.remove() }
        let file = home.folder.appending(path: "Bookmarks")
        try BookmarksFixture.chromium.write(to: file)
        let bookmarks = ProfileParseJob(.chromiumBookmarks, file: file)

        var huge = ImportSkipCounts()
        huge.notImportable = .max
        huge.badTime = 1
        await #expect(throws: BrowserImportServiceError.badReply) {
            _ = try await HostileImportService.reader(.parsed(.bookmarks(ImportRead(skipped: huge)))).parse(bookmarks)
        }
        var negative = ImportSkipCounts()
        negative.overLimit = -1
        await #expect(throws: BrowserImportServiceError.badReply) {
            _ = try await HostileImportService.reader(.parsed(.bookmarks(ImportRead(skipped: negative)))).parse(bookmarks)
        }
        await #expect(throws: BrowserImportServiceError.badReply) {
            _ = try await HostileImportService.reader(.parsed(.history(ImportRead()))).parse(bookmarks)
        }

        let script = try #require(URL(string: "javascript:alert(document.cookie)"))
        let internal = try #require(URL(string: "chrome://settings"))
        let page = try #require(URL(string: "https://example.com/"))
        let injected = ImportRead(items: [ImportedBookmark(title: "x", url: script), ImportedBookmark(title: "y", url: internal),
                                          ImportedBookmark(title: "z", url: page)])
        let cleaned = try await HostileImportService.reader(.parsed(.bookmarks(injected))).parse(bookmarks)
        guard case .bookmarks(let read) = cleaned else {
            Issue.record("a bookmarks job answered \(cleaned.kind)")
            return
        }
        #expect(read.items.map(\.url) == [page])
        #expect(read.skipped.notImportable == 2)

        let history = ProfileParseJob(spec: ProfileParseSpec(format: .chromiumHistory, historyLimit: 2), file: file)
        let now = Date(timeIntervalSinceReferenceDate: 800_000_000)
        var loud = ImportedHistoryEntry(url: page, title: nil, visitCount: 1, lastVisit: now)
        loud.visitCount = .max
        var timeless = ImportedHistoryEntry(url: page, title: nil, visitCount: 1, lastVisit: now)
        timeless.lastVisit = .distantFuture.addingTimeInterval(1e12)
        let pages = ImportRead(items: [loud, timeless, loud, loud])
        guard case .history(let visits) = try await HostileImportService.reader(.parsed(.history(pages))).parse(history) else {
            Issue.record("a history job answered another kind")
            return
        }
        #expect(visits.items.map(\.visitCount) == [ImportedHistoryEntry.maximumVisitCount])
        #expect(visits.skipped.badTime == 1)
    }

    /// A cookie reader's typed error crosses as itself, so the import reports it as it did in process.
    @Test func aCookieReadersErrorCrossesAsItself() async throws {
        let home = try ImportServiceScratch()
        defer { home.remove() }
        let file = home.folder.appending(path: "Cookies")
        try SQLiteFixture.create(file, "CREATE TABLE cookies (name, value);")
        await #expect(throws: CookieImportError.malformed("cookies table")) {
            _ = try await BrowserImportServiceReader(connector: AnonymousImportService()).cookies(.chromiumCookies, file: file)
        }
    }

    /// Bytes that are not a parse spec get a typed failure, and the service keeps serving.
    @Test func aRequestThatIsNotASpecIsRefused() throws {
        let home = try ImportServiceScratch()
        defer { home.remove() }
        let file = home.folder.appending(path: "Bookmarks")
        try BookmarksFixture.chromium.write(to: file)
        let handler = BrowserImportParseHandler(scratch: home.folder)
        let main = try FileHandle(forReadingFrom: file)
        defer { try? main.close() }
        #expect(handler.handle(request: Data("not a plist".utf8), main: main, wal: nil, journal: nil) == .failed(.badRequest))
        let spec = ProfileParseSpec(format: .chromiumBookmarks)
        let expected = try spec.parse(at: file)
        let request = try BrowserImportServiceWire().encode(spec)
        let reply = handler.handle(request: request, main: main, wal: nil, journal: nil)
        #expect(reply == .parsed(expected))
    }

    /// A file the reader refuses comes back as the reader's error, not a trap or an empty read.
    @Test func aFileTheReaderRefusesComesBackAsItsError() async throws {
        let home = try ImportServiceScratch()
        defer { home.remove() }
        let file = home.folder.appending(path: "History")
        try Data("not a database".utf8).write(to: file)
        let job = ProfileParseJob(.chromiumHistory, file: file)
        await #expect(throws: BrowserImportServiceError.self) {
            _ = try await BrowserImportServiceReader(connector: AnonymousImportService()).parse(job)
        }
    }

    /// Bytes that do not decode are refused, and a value of another kind than the format reads fails its check.
    @Test func aReplyOfTheWrongShapeIsRefused() throws {
        let wire = BrowserImportServiceWire()
        #expect(throws: BrowserImportServiceError.badReply) { try wire.reply(from: Data([0xFF, 0x00])) }
        let history = wire.encode(.parsed(.history(ImportRead())))
        guard case .parsed(let result) = try wire.reply(from: history) else {
            Issue.record("an encoded history reply did not decode")
            return
        }
        #expect(result.checked(for: ProfileParseSpec(format: .chromiumBookmarks)) == nil)
        #expect(result.checked(for: ProfileParseSpec(format: .safariHistory)) == result)
    }

    /// No path crosses to the service: the request names the reader only.
    @Test func theRequestCarriesNoPath() throws {
        let path = "/Users/someone/Library/Application Support/Google/Chrome/Default/History"
        let job = try #require(ProfileParseJob.history(.chromium, file: URL(fileURLWithPath: path), limit: 10))
        let request = try BrowserImportServiceWire().encode(job.spec)
        #expect(request.range(of: Data("someone".utf8)) == nil)
        #expect(try BrowserImportServiceWire().spec(from: request) == job.spec)
    }

    /// A build without the service parses nothing: the App has no parser of its own to fall back to.
    @Test func aBuildWithoutTheServiceParsesNothing() async throws {
        let home = try ImportServiceScratch()
        defer { home.remove() }
        let file = home.folder.appending(path: "Bookmarks")
        try BookmarksFixture.chromium.write(to: file)
        let reader = BrowserImportServiceReader(connector: BundledBrowserImportService(app: home.folder))
        await #expect(throws: BrowserImportServiceError.notBundled) {
            _ = try await reader.parse(ProfileParseJob(.chromiumBookmarks, file: file))
        }
    }
}

/// The parse jobs of the formats a fixture profile does not hold as one file of its own.
enum ImportServiceJobs {
    static func extra(in folder: URL) throws -> [ProfileParseJob] {
        let arc = folder.appending(path: "StorableSidebar.json")
        try BookmarksFixture.arc.write(to: arc)
        let preferences = folder.appending(path: "Preferences")
        try Data(#"""
            {"default_search_provider":{"guid":"g1"},
             "extensions":{"settings":{"aapbdbdomjkkjkaonfhkkikfgjllcleb":{"location":1,"from_webstore":true,"state":1,
               "manifest":{"name":"Translate","version":"2.0"}}}}}
            """#.utf8).write(to: preferences)
        let webData = folder.appending(path: "Web Data")
        try SQLiteFixture.create(webData, """
            CREATE TABLE keywords (short_name, keyword, url, suggest_url, sync_guid);
            INSERT INTO keywords VALUES ('Kagi', 'kagi', 'https://kagi.com/search?q={searchTerms}', '', 'g1');
            """)
        let search = folder.appending(path: "search.json.mozlz4")
        try MozLz4Fixture.file(Data(#"{"engines":[{"id":"ddg","_name":"DuckDuckGo"}],"metaData":{"defaultEngineId":"ddg"}}"#.utf8))
            .write(to: search)
        let manifest = folder.appending(path: "manifest.json")
        try Data(#"{"name":"__MSG_name__","version":"1.2","default_locale":"en"}"#.utf8).write(to: manifest)
        let messages = folder.appending(path: "messages.json")
        try Data(#"{"Name":{"message":"Dark Reader"}}"#.utf8).write(to: messages)
        let archive = folder.appending(path: "export.zip")
        try ZipFixture.sample.write(to: archive)
        return [
            ProfileParseJob(spec: ProfileParseSpec(format: .arcSidebar, profileDirectory: "Default"), file: arc),
            ProfileParseJob(.chromiumPreferences, file: preferences),
            ProfileParseJob(.chromiumSearchEngine, file: webData, key: "g1"),
            ProfileParseJob(.firefoxSearchEngine, file: search),
            ProfileParseJob(.chromiumExtensionManifest, file: manifest),
            ProfileParseJob(.chromiumExtensionMessage, file: messages, key: "name"),
            ProfileParseJob(.exportArchive, file: archive),
        ]
    }

    /// The cookie and password files of a fixture profile.
    static func secrets(_ profile: BrowserSourceProfile) -> [ProfileParseJob] {
        let path = profile.path
        switch profile.browser.family {
        case .chromium:
            var jobs = BrowserSourceDetector.chromiumCookieFile(path).map { [ProfileParseJob(.chromiumCookies, file: $0)] } ?? []
            #if !CMUX_NO_PASSWORD_IMPORT
            jobs += ChromiumLoginDataReader().databases(profile: path).map { ProfileParseJob(.chromiumLogins, file: $0) }
            #endif
            return jobs
        case .firefox:
            var jobs = [ProfileParseJob(.firefoxCookies, file: path.appending(path: "cookies.sqlite"))]
            #if !CMUX_NO_PASSWORD_IMPORT
            jobs += [ProfileParseJob(.firefoxKeyStore, file: path.appending(path: "key4.db")),
                     ProfileParseJob(.firefoxLogins, file: path.appending(path: "logins.json"))]
            #endif
            return jobs
        case .safari:
            guard let relative = profile.browser.safariCookieFile else { return [] }
            let home = path.deletingLastPathComponent().deletingLastPathComponent()
            return [ProfileParseJob(.safariCookies, file: home.appending(path: relative))]
        case .webkit, .other:
            return []
        }
    }
}

/// The service in the test process: an anonymous XPC listener with the service's own gate.
struct AnonymousImportService: BrowserImportServiceConnecting {
    // crash-allow: made and resumed in init, then only read; NSXPCListener's endpoint and connection creation are thread-safe.
    private nonisolated(unsafe) let listener: NSXPCListener
    private let gate = BrowserImportServiceGate()

    init() {
        listener = NSXPCListener.anonymous()
        listener.delegate = gate
        listener.resume()
    }

    func isAvailable() -> Bool { true }

    func makeConnection() -> NSXPCConnection {
        NSXPCConnection(listenerEndpoint: listener.endpoint)
    }
}

/// A compromised service: an anonymous listener whose exported object answers every parse with one
/// fixed reply, encoded as the real service encodes its replies.
struct HostileImportService: BrowserImportServiceConnecting {
    // crash-allow: made and resumed in init, then only read; NSXPCListener's endpoint and connection creation are thread-safe.
    private nonisolated(unsafe) let listener: NSXPCListener
    private let gate: HostileGate

    init(reply: BrowserImportServiceReply) {
        gate = HostileGate(answer: BrowserImportServiceWire().encode(reply))
        listener = NSXPCListener.anonymous()
        listener.delegate = gate
        listener.resume()
    }

    /// The App's reader, connected to a service that answers `reply`.
    static func reader(_ reply: BrowserImportServiceReply) -> BrowserImportServiceReader {
        BrowserImportServiceReader(connector: HostileImportService(reply: reply))
    }

    func isAvailable() -> Bool { true }

    func makeConnection() -> NSXPCConnection {
        NSXPCConnection(listenerEndpoint: listener.endpoint)
    }

    final class HostileGate: NSObject, NSXPCListenerDelegate, BrowserImportParsing, Sendable {
        let answer: Data

        init(answer: Data) {
            self.answer = answer
        }

        func listener(_ listener: NSXPCListener, shouldAcceptNewConnection connection: NSXPCConnection) -> Bool {
            connection.exportedInterface = NSXPCInterface.browserImportParsing()
            connection.exportedObject = self
            connection.resume()
            return true
        }

        func parse(_ request: Data, main: FileHandle, wal: FileHandle?, journal: FileHandle?, reply: @escaping @Sendable (Data) -> Void) {
            reply(answer)
        }
    }
}

/// A new empty folder for one test.
struct ImportServiceScratch {
    let folder: URL

    init() throws {
        folder = FileManager.default.temporaryDirectory
            .appending(path: "cmux-import-service-test-\(UUID().uuidString)", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
    }

    func remove() {
        try? FileManager.default.removeItem(at: folder)
    }
}
