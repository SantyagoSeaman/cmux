public import CmuxNextBrowserImport
import CmuxNextCompat
import CmuxSafeConcurrency
public import Foundation

/// The App's only way to parse another browser's files: in the sandboxed import service. It opens
/// the job's file (and a database's side files) read-only here, where the folder grant is held,
/// and sends the open files and the spec; the service sends back values, which are decoded as the
/// wire's types only and then checked against the readers' invariants
/// (`ProfileParseResult.checked(for:)`), as Chrome's browser process checks what its utility
/// importer sends (chrome/browser/importer/external_process_importer_client.cc). One connection per
/// parse, so a parse that crashed the service leaves nothing behind for the next.
public struct BrowserImportServiceReader: ProfileDataReading {
    private let connector: any BrowserImportServiceConnecting

    public init(connector: any BrowserImportServiceConnecting = BundledBrowserImportService()) {
        self.connector = connector
    }

    @concurrent public func parse(_ job: ProfileParseJob) async throws -> ProfileParseResult {
        guard connector.isAvailable() else { throw BrowserImportServiceError.notBundled }
        let wire = BrowserImportServiceWire()
        let request = try wire.encode(job.spec)
        let files = try ServiceFiles(job)
        defer { files.close() }
        let bytes = try await send(request, files)
        switch try wire.reply(from: bytes) {
        case .parsed(let result):
            guard let checked = result.checked(for: job.spec) else { throw BrowserImportServiceError.badReply }
            return checked
        case .failed(.cookie(let error)):
            throw error
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
