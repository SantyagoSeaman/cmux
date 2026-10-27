public import CmuxNextBrowserImport
public import Foundation

/// Why the service could not parse a request.
public enum BrowserImportServiceFailure: Error, Sendable, Codable, Equatable, CustomStringConvertible {
    /// The request was not a parse spec.
    case badRequest
    /// The file the app sent could not be copied into the service's folder.
    case staging
    /// The reader refused the file: its error, as the in-process import reports it.
    case reader(String)
    /// The result could not be encoded.
    case encoding

    /// Longest reader message that crosses back; a longer one is cut.
    static let maximumMessage = 1024

    public var description: String {
        switch self {
        case .badRequest: "the import service could not read the request"
        case .staging: "the import service could not read the file"
        case .reader(let message): message
        case .encoding: "the import service could not encode the result"
        }
    }
}

/// The service's answer to one parse.
public enum BrowserImportServiceReply: Sendable, Codable, Equatable {
    case parsed(ProfileParseResult)
    case failed(BrowserImportServiceFailure)
}

/// Binary property lists both ways: exact doubles (visit times), and a decoder that only builds
/// the declared types, so a reply from a compromised service is a value or a decoding error.
struct BrowserImportServiceWire {
    func encode(_ spec: ProfileParseSpec) throws -> Data {
        try Self.encoder().encode(spec)
    }

    func spec(from request: Data) throws -> ProfileParseSpec {
        try PropertyListDecoder().decode(ProfileParseSpec.self, from: request)
    }

    /// The reply as bytes; a reply that does not encode becomes `.failed(.encoding)`, and empty bytes,
    /// which the app reads as a bad reply, only if that does not encode either.
    func encode(_ reply: BrowserImportServiceReply) -> Data {
        let encoder = Self.encoder()
        if let data = try? encoder.encode(reply) { return data }
        return (try? encoder.encode(BrowserImportServiceReply.failed(.encoding))) ?? Data()
    }

    func reply(from data: Data) throws(BrowserImportServiceError) -> BrowserImportServiceReply {
        do {
            return try PropertyListDecoder().decode(BrowserImportServiceReply.self, from: data)
        } catch {
            throw BrowserImportServiceError.badReply
        }
    }

    private static func encoder() -> PropertyListEncoder {
        let encoder = PropertyListEncoder()
        encoder.outputFormat = .binary
        return encoder
    }
}
