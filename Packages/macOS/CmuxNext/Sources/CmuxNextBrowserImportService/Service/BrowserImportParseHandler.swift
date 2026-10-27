public import CmuxNextBrowserImport
import CmuxNextCompat
public import Foundation

/// The service side of one parse: copies the files the app sent into a private folder, runs the
/// same `ProfileParseSpec.parse(at:)` the in-process import runs, and answers with the values or a
/// typed failure. It never throws and never traps on what it was sent; the folder goes when the
/// reply is made.
public struct BrowserImportParseHandler: Sendable {
    /// Where each request's private folder is made: the service's own temporary folder (in its
    /// sandbox container).
    private let scratch: URL

    public init(scratch: URL = FileManager.default.temporaryDirectory) {
        self.scratch = scratch
    }

    /// The reply to one request, encoded for the wire.
    public func reply(request: Data, main: FileHandle, wal: FileHandle?, journal: FileHandle?) -> Data {
        BrowserImportServiceWire().encode(handle(request: request, main: main, wal: wal, journal: journal))
    }

    func handle(request: Data, main: FileHandle, wal: FileHandle?, journal: FileHandle?) -> BrowserImportServiceReply {
        guard let spec = try? BrowserImportServiceWire().spec(from: request) else { return .failed(.badRequest) }
        let folder = scratch.appending(path: "cmux-import-service-\(UUID().uuidString)", directoryHint: .isDirectory)
        defer { try? FileManager.checked.removeItem(at: folder) }
        let file = folder.appending(path: "source")
        do {
            try FileManager.checked.createDirectory(at: folder, withIntermediateDirectories: true)
            try Self.copy(main, to: file)
        } catch {
            return .failed(.staging)
        }
        // Side files as SQLiteSnapshot treats them: copied when they come, a database without them still read.
        if spec.format.isDatabase {
            if let wal { try? Self.copy(wal, to: URL(fileURLWithPath: file.path + "-wal")) }
            if let journal { try? Self.copy(journal, to: URL(fileURLWithPath: file.path + "-journal")) }
        }
        do {
            return .parsed(try spec.parse(at: file))
        } catch {
            return .failed(.reader(String(String(describing: error).prefix(BrowserImportServiceFailure.maximumMessage))))
        }
    }

    /// Streams `source` into a new file at `destination`, one megabyte at a time.
    static func copy(_ source: FileHandle, to destination: URL) throws {
        guard FileManager.checked.createFile(atPath: destination.path, contents: nil) else {
            throw CocoaError(.fileWriteUnknown)
        }
        let output = try FileHandle.blockingForWriting(to: destination)
        defer { try? output.close() }
        while let chunk = try source.blockingRead(upToCount: 1 << 20), !chunk.isEmpty {
            try output.blockingWrite(contentsOf: chunk)
        }
    }
}

/// The service's exported object: each call is answered on the connection's own XPC queue, in the
/// service process, so a slow parse holds only that connection.
public final nonisolated class BrowserImportService: NSObject, BrowserImportParsing, Sendable {
    private let handler: BrowserImportParseHandler

    public init(handler: BrowserImportParseHandler = BrowserImportParseHandler()) {
        self.handler = handler
    }

    public func parse(_ request: Data, main: FileHandle, wal: FileHandle?, journal: FileHandle?,
                      reply: @escaping @Sendable (Data) -> Void) {
        reply(handler.reply(request: request, main: main, wal: wal, journal: journal))
    }
}

/// The listener's delegate: exports the service on each new connection. Only the app that carries
/// the service can reach it (launchd scopes an app's XPC services to its bundle), so it checks no
/// code signature; what it is sent is parsed as untrusted bytes either way.
public final nonisolated class BrowserImportServiceGate: NSObject, NSXPCListenerDelegate, Sendable {
    private let service: BrowserImportService

    public init(service: BrowserImportService = BrowserImportService()) {
        self.service = service
    }

    public func listener(_ listener: NSXPCListener, shouldAcceptNewConnection connection: NSXPCConnection) -> Bool {
        connection.exportedInterface = NSXPCInterface.browserImportParsing()
        connection.exportedObject = service
        connection.resume()
        return true
    }
}
