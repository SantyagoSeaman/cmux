import Foundation

/// One `app-child-read` answer: output from `after` (whole UTF-8 characters only while the child
/// runs), the offset to read from next, whether output before `after` was dropped, and the exit once
/// the child ended and every byte was read.
public struct AppChildChunk: Decodable, Sendable, Equatable {
    public struct Exit: Decodable, Sendable, Equatable {
        public var code: Int32?
        public var signal: Int32?

        public init(code: Int32? = nil, signal: Int32? = nil) {
            self.code = code
            self.signal = signal
        }
    }

    public var output: String
    public var next: UInt64
    public var truncated: Bool
    public var exit: Exit?

    enum CodingKeys: String, CodingKey {
        case output, next, truncated, exit
    }

    public init(output: String, next: UInt64, truncated: Bool = false, exit: Exit? = nil) {
        self.output = output
        self.next = next
        self.truncated = truncated
        self.exit = exit
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        output = try container.decode(String.self, forKey: .output)
        next = try container.decode(UInt64.self, forKey: .next)
        truncated = try container.decodeIfPresent(Bool.self, forKey: .truncated) ?? false
        exit = try container.decodeIfPresent(Exit.self, forKey: .exit)
    }
}
