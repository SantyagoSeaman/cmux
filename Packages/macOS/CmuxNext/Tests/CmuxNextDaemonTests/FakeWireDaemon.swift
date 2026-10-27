import CmuxNextCompat
import Darwin
import Foundation
@testable import CmuxNextDaemon

/// A Unix socket served by this test process that speaks just enough raw protocol v12 for control,
/// page relay and byte-mode terminal stream connections: `identify` (after one line that is not
/// JSON; it offers `origin-claim-v1`, and `attach-streams-v1` unless it plays an older daemon), `client-hello`
/// with a connection id, nothing for ``unanswered``, `ok:true` for every other request, and after each
/// `attach-surface` reply an optional `colors-changed`, then `outputBytesPerAttach` bytes of `output` events
/// in 64 KiB chunks, each JSON event tagged with the request's `attach_stream` when it named one.
final class FakeWireDaemon: Sendable {
    static let chunkBytes = 64 << 10
    /// A command the fake never answers.
    static let unanswered = "never-reply"

    let path: String
    private let listener: Int32
    /// Every request line the fake read, in order (all connections).
    let requests = RequestLog()

    /// The request lines a fake daemon read.
    final class RequestLog: Sendable {
        private let lines = Mutex<[Data]>([])

        func append(_ line: Data) { lines.withLock { $0.append(line) } }

        /// The requests named `command`, decoded.
        func named(_ command: String) -> [[String: Any]] {
            lines.withLock { $0 }.compactMap { line in
                guard let request = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                      request["cmd"] as? String == command else { return nil }
                return request
            }
        }
    }

    static func chunks(for bytes: Int) -> Int { (bytes + chunkBytes - 1) / chunkBytes }
    static func base64Bytes(of bytes: Int) -> Int { (bytes + 2) / 3 * 4 }

    /// - Parameter servesWire: Also offers `attach-frames-v1` and `terminal-output-ring-v1` (a
    ///   current local daemon); the fake still sends JSON events only.
    init(outputBytesPerAttach: Int, colorsBeforeOutput: Bool = false, servesStreams: Bool = true,
         servesWire: Bool = false) throws {
        path = "/tmp/cnd-wire-\(UUID().uuidString.prefix(8).lowercased()).sock"
        let listener = socket(AF_UNIX, SOCK_STREAM, 0)  // a local: the closures below may not capture self
        self.listener = listener
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8)
        withUnsafeMutableBytes(of: &address.sun_path) { buffer in
            buffer.copyBytes(from: pathBytes)
            buffer[pathBytes.count] = 0
        }
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(listener, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0, listen(listener, 8) == 0 else {
            close(listener)
            throw POSIXError(.EADDRINUSE)
        }
        let script = Script(outputBytes: outputBytesPerAttach, colors: colorsBeforeOutput, streams: servesStreams,
                            wire: servesWire, log: requests)
        let thread = Thread {
            while true {
                let client = accept(listener, nil, nil)
                guard client >= 0 else { return }
                let serving = Thread { Self.serve(client, script: script) }
                serving.qualityOfService = .utility
                serving.start()
            }
        }
        thread.qualityOfService = .utility
        thread.start()
    }

    func stop() {
        shutdown(listener, SHUT_RDWR)
        close(listener)
        unlink(path)
    }

    /// What every connection is sent after its `attach-surface` reply.
    private struct Script: Sendable {
        var outputBytes: Int
        var colors: Bool
        /// Offers `attach-streams-v1` (false: an older daemon).
        var streams: Bool
        /// Offers `attach-frames-v1` and `terminal-output-ring-v1` too.
        var wire: Bool
        var log: RequestLog
    }

    private static func serve(_ client: Int32, script: Script) {
        defer { close(client) }
        var on: Int32 = 1
        setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 64 << 10)
        while true {
            let count = recv(client, &chunk, chunk.count, 0)
            guard count > 0 else { return }
            buffer.append(contentsOf: chunk[0..<count])
            while let newline = buffer.firstIndex(of: 0x0A) {
                let line = Data(buffer[buffer.startIndex..<newline])
                buffer.removeSubrange(buffer.startIndex...newline)
                script.log.append(line)
                guard let request = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                      let id = request["id"] as? Int, let command = request["cmd"] as? String else { continue }
                guard answer(command, id: id, request: request, to: client, script: script) else { return }
            }
        }
    }

    private static func answer(_ command: String, id: Int, request: [String: Any], to client: Int32, script: Script) -> Bool {
        switch command {
        case unanswered:
            return true
        case IdentifyRequest.command:
            let capabilities = [DaemonClientRole.originClaimCapability]
                + (script.streams ? [DaemonCapability.attachStreams.rawValue] : [])
                + (script.wire ? [DaemonCapability.attachFrames.rawValue, DaemonCapability.terminalOutputRing.rawValue] : [])
            let identity: [String: Any] = ["app": "cmux-tui", "protocol": 12, "generation": "gen-wire",
                                           "capabilities": capabilities]
            return write(Data("{]".utf8), to: client) && reply(id, data: identity, to: client)
        case "client-hello":
            return reply(id, data: ["connection_id": "conn-\(id)"], to: client)
        case AttachSurfaceRequest.command:
            guard reply(id, data: ["lease": "lease-\(id)"], to: client) else { return false }
            let surface = request["surface"] as? Int ?? 0
            let tag: [String: Any] = (request["attach_stream"] as? Int).map { ["attach_stream": $0] } ?? [:]
            let colors: [String: Any] = ["event": "colors-changed", "surface": surface, "fg": "#ffffff"]
            if script.colors, !send(colors.merging(tag, uniquingKeysWith: { $1 }), to: client) { return false }
            var sent = 0
            while sent < script.outputBytes {
                let size = min(chunkBytes, script.outputBytes - sent)
                let event: [String: Any] = ["event": "output", "surface": surface,
                                            "data": Data(repeating: 0x61, count: size).base64EncodedString()]
                guard send(event.merging(tag, uniquingKeysWith: { $1 }), to: client) else { return false }
                sent += size
            }
            return true
        default:
            return reply(id, data: [String: Any](), to: client)
        }
    }

    private static func reply(_ id: Int, data: [String: Any], to client: Int32) -> Bool {
        send(["id": id, "ok": true, "data": data], to: client)
    }

    private static func send(_ object: [String: Any], to client: Int32) -> Bool {
        guard let line = try? JSONSerialization.data(withJSONObject: object) else { return false }
        return write(line, to: client)
    }

    /// Writes `line` and its newline whole.
    private static func write(_ line: Data, to client: Int32) -> Bool {
        var bytes = line
        bytes.append(0x0A)
        return bytes.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return false }
            var offset = 0
            while offset < raw.count {
                let written = Darwin.send(client, base + offset, raw.count - offset, 0)
                if written < 0, errno == EINTR { continue }
                guard written > 0 else { return false }
                offset += written
            }
            return true
        }
    }
}
