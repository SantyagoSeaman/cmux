import CmuxFuzzSupport
@testable import CmuxNextBrowserImport
@testable import CmuxNextBrowserImportService
import Foundation
import Testing

/// The sandboxed import service (p10-stability-208, plans/cmux-next/rule-of-2.md) over a real XPC
/// connection: an anonymous listener in the test process exports the same ``BrowserImportServiceGate``
/// the service's main runs, so the files cross as descriptors and the values as the wire's bytes.
/// The import must read the same through it as in process, and the fuzz of task 025 runs against it.
/// Gated like the fuzz: agents run it on the fleet, CI does not (Lawrence 2026-10-23):
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
        let arc = home.folder.appending(path: "StorableSidebar.json")
        try BookmarksFixture.arc.write(to: arc)
        var jobs = [ProfileParseJob(spec: ProfileParseSpec(format: .arcSidebar, profileDirectory: "Default"), file: arc)]
        for profile in profiles.profiles {
            jobs += [ProfileParseJob.bookmarks(profile.source), ProfileParseJob.openTabs(profile.source)].compactMap { $0 }
            if let file = profile.source.historyFile,
               let job = ProfileParseJob.history(profile.source.browser.family, file: file, limit: ImportPlan.defaultHistoryLimit) {
                jobs.append(job)
            }
        }
        #expect(Set(jobs.map(\.spec.format)) == Set(ProfileParseSpec.Format.allCases))
        let service = BrowserImportServiceReader(connector: AnonymousImportService())
        for job in jobs {
            let local = try await InProcessProfileReader().parse(job)
            let remote = try await service.parse(job)
            #expect(remote == local, "\(job.spec.format.rawValue) read differently through the service")
        }
    }

    /// Whole profiles with damaged files, as ``BrowserImportFuzzTests/damagedProfilesAreReportedOneByOne()``,
    /// parsed by the service: each profile is reported once, an undamaged one imports in full.
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
        let job = ProfileParseJob(spec: ProfileParseSpec(format: .chromiumHistory), file: file)
        await #expect(throws: BrowserImportServiceError.self) {
            _ = try await BrowserImportServiceReader(connector: AnonymousImportService()).parse(job)
        }
    }

    /// The app trusts no reply: bytes that do not decode, and a value of another kind than the
    /// format reads, are refused.
    @Test func aReplyOfTheWrongShapeIsRefused() throws {
        let wire = BrowserImportServiceWire()
        #expect(throws: BrowserImportServiceError.badReply) { try wire.reply(from: Data([0xFF, 0x00])) }
        let history = wire.encode(.parsed(.history(ImportRead())))
        guard case .parsed(let result) = try wire.reply(from: history) else {
            Issue.record("an encoded history reply did not decode")
            return
        }
        #expect(!result.matches(.chromiumBookmarks))
        #expect(result.matches(.safariHistory))
    }

    /// No path crosses to the service: the request names the reader only.
    @Test func theRequestCarriesNoPath() throws {
        let path = "/Users/someone/Library/Application Support/Google/Chrome/Default/History"
        let job = try #require(ProfileParseJob.history(.chromium, file: URL(fileURLWithPath: path), limit: 10))
        let request = try BrowserImportServiceWire().encode(job.spec)
        #expect(request.range(of: Data("someone".utf8)) == nil)
        #expect(try BrowserImportServiceWire().spec(from: request) == job.spec)
    }

    /// A build without the service parses in process.
    @Test func aBuildWithoutTheServiceParsesInProcess() async throws {
        let home = try ImportServiceScratch()
        defer { home.remove() }
        let file = home.folder.appending(path: "Bookmarks")
        try BookmarksFixture.chromium.write(to: file)
        let job = ProfileParseJob(spec: ProfileParseSpec(format: .chromiumBookmarks), file: file)
        let reader = BrowserImportServiceReader(connector: BundledBrowserImportService(app: home.folder))
        let local = try await InProcessProfileReader().parse(job)
        let read = try await reader.parse(job)
        #expect(read == local)
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
