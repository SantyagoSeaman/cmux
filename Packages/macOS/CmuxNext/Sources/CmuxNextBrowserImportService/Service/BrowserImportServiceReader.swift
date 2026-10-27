public import CmuxNextBrowserImport
import CmuxNextCompat
import CmuxSafeConcurrency
public import Foundation
import os

/// Parses in the sandboxed import service: opens the job's file (and a database's side files)
/// read-only here, where the folder grant is held, and sends the open files and the spec; the
/// service sends back values, which are decoded as the wire's types only. One connection per
/// parse, so a parse that crashed the service leaves nothing behind for the next.
///
/// Feature `browserImportSandboxedService` picks it (``preferred(sandboxed:connector:)``). A build
/// without the service parses in process, and says so in the log.
public struct BrowserImportServiceReader: ProfileDataReading {
    private static let log = Logger(subsystem: "com.cmuxterm.app", category: "BrowserImportService")

    private let connector: any BrowserImportServiceConnecting

    public init(connector: any BrowserImportServiceConnecting = BundledBrowserImportService()) {
        self.connector = connector
    }

    /// The reader an import uses: the sandboxed service when `sandboxed`, else the in-process parse.
    public static func preferred(sandboxed: Bool,
                                 connector: any BrowserImportServiceConnecting = BundledBrowserImportService()) -> any ProfileDataReading {
        sandboxed ? BrowserImportServiceReader(connector: connector) : InProcessProfileReader()
    }

    public func parse(_ job: ProfileParseJob) async throws -> ProfileParseResult {
        guard connector.isAvailable() else {
            Self.log.notice("browser import service is not in this build; parsing \(job.spec.format.rawValue, privacy: .public) in process")
            return try await InProcessProfileReader().parse(job)
        }
        let wire = BrowserImportServiceWire()
        let request = try wire.encode(job.spec)
        let files = try ServiceFiles(job)
        defer { files.close() }
        let bytes = try await send(request, files)
        switch try wire.reply(from: bytes) {
        case .parsed(let result):
            guard result.matches(job.spec.format) else { throw ProfileParseMismatch(format: job.spec.format) }
            return result
        case .failed(let failure):
            throw BrowserImportServiceError.service(failure)
        }
    }

    private func send(_ request: Data, _ files: ServiceFiles) async throws -> Data {
        let connection = connector.makeConnection()
        connection.remoteObjectInterface = NSXPCInterface.browserImportParsing()
        connection.resume()
        // Also ends the service's side of a cancelled wait; the service finishes that parse unobserved.
        defer { connection.invalidate() }
        // XPC may call both the reply and the error handler: the first resume wins.
        return try await withOnceThrowingContinuation(cancellation: .cancellable) { (once: OnceContinuation<Data, any Error>) in
            let proxy = connection.remoteObjectProxyWithErrorHandler { error in
                once.resume(throwing: BrowserImportServiceError.connection((error as NSError).code))
            }
            guard let service = proxy as? any BrowserImportParsing else {
                once.resume(throwing: BrowserImportServiceError.badReply)
                return
            }
            service.parse(request, main: files.main, wal: files.wal, journal: files.journal) { once.resume(returning: $0) }
        }
    }
}

/// The files of one parse, open for reading. XPC sends a duplicate of each descriptor, so they are
/// closed once the call is made.
private struct ServiceFiles {
    let main: FileHandle
    let wal: FileHandle?
    let journal: FileHandle?

    init(_ job: ProfileParseJob) throws {
        main = try FileHandle.blockingForReading(from: job.file)
        // As SQLiteSnapshot copies them: a side file that is missing or will not open is left out.
        wal = job.spec.format.isDatabase ? try? FileHandle.blockingForReading(from: URL(fileURLWithPath: job.file.path + "-wal")) : nil
        journal = job.spec.format.isDatabase ? try? FileHandle.blockingForReading(from: URL(fileURLWithPath: job.file.path + "-journal")) : nil
    }

    func close() {
        try? main.close()
        try? wal?.close()
        try? journal?.close()
    }
}
