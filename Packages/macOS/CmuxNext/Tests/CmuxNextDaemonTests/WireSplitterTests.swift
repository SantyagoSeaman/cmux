import CmuxNextIPC
import Foundation
import Testing
@testable import CmuxNextDaemon

/// The shared connection's byte stream carries JSON lines and binary attach
/// frames (`attach-frames-v1`); the transport's reader (CmuxNextIPC's
/// ``FrameReader``) cuts them apart at any read boundary, and each frame's
/// header reads back as the ``AttachFrameHeader`` the daemon wrote.
@Suite struct WireSplitterTests {
    /// One message as the transport routes it.
    enum Cut: Equatable {
        case line(Data)
        case frame(AttachFrameHeader, Data)
    }

    private func frame(_ kind: AttachFrameKind, stream: UInt16, offset: UInt64, _ payload: Data) -> Data {
        AttachFrameHeader(kind: kind, stream: stream, length: payload.count, offset: offset).encoded + payload
    }

    private func split(_ wire: Data, chunk: Int) throws -> [Cut] {
        var reader = FrameReader()
        reader.negotiateFrames()
        var cuts: [Cut] = []
        var index = wire.startIndex
        while index < wire.endIndex {
            let end = min(index + chunk, wire.endIndex)
            wire[index..<end].withUnsafeBytes { raw in reader.push(raw) }
            while let message = try reader.next() {
                switch message {
                case .json(let bytes) where bytes.isEmpty: break
                case .json(let bytes): cuts.append(.line(Data(bytes)))
                case .frame(let frame):
                    cuts.append(.frame(try AttachFrameHeader(frame.header.encode()), Data(frame.payload)))
                }
            }
            index = end
        }
        return cuts
    }

    @Test func linesAndFramesComeApartAtEveryReadBoundary() throws {
        // A payload holding newlines and 0xFF: a frame is never searched for either.
        let output = Data("ls\r\n\u{1b}[1m".utf8) + Data([0xFF, 0x0A, 0x00])
        let wire = Data(#"{"id":1,"ok":true}"#.utf8) + Data([0x0A])
            + frame(.output, stream: 513, offset: 1 << 33, output)
            + Data("\n".utf8)  // an empty line between messages carries nothing
            + frame(.ringAdvance, stream: 2, offset: 9, Data())
            + Data(#"{"event":"x","attach_stream":2}"#.utf8) + Data([0x0A])
        let expected: [Cut] = [
            .line(Data(#"{"id":1,"ok":true}"#.utf8)),
            .frame(AttachFrameHeader(kind: .output, stream: 513, length: output.count, offset: 1 << 33), output),
            .frame(AttachFrameHeader(kind: .ringAdvance, stream: 2, length: 0, offset: 9), Data()),
            .line(Data(#"{"event":"x","attach_stream":2}"#.utf8)),
        ]
        for chunk in 1...wire.count {
            #expect(try split(wire, chunk: chunk) == expected, "read size \(chunk)")
        }
    }

    @Test func aMalformedHeaderEndsTheStream() {
        var bad = AttachFrameHeader(kind: .output, stream: 1, length: 0, offset: 0).encoded
        bad[1] = 99
        #expect(throws: BadMessage.unknownKind(99)) { _ = try split(bad, chunk: bad.count) }
        var zeroStream = AttachFrameHeader(kind: .output, stream: 1, length: 0, offset: 0).encoded
        zeroStream[2] = 0
        #expect(throws: BadMessage.dataOnControlStream(kind: AttachFrameKind.output.rawValue)) {
            _ = try split(zeroStream, chunk: zeroStream.count)
        }
    }

    @Test func aFrameBeforeAnyStreamIsABadMessage() {
        let wire = frame(.output, stream: 1, offset: 0, Data("x".utf8))
        var reader = FrameReader()
        wire.withUnsafeBytes { raw in reader.push(raw) }
        #expect(throws: BadMessage.framesNotNegotiated) { _ = try reader.next() }
    }

    @Test func aFrameMetaSplitsIntoItsJSONAndItsData() throws {
        let meta = Data(#"{"phase":"ready"}"#.utf8)
        var payload = Data([UInt8(meta.count), 0, 0, 0]) + meta + Data([0xFF, 1, 2])
        let parts = try #require(AttachFrameMeta(payload))
        #expect(parts.meta == meta)
        #expect(parts.data == Data([0xFF, 1, 2]))
        payload[0] = 200
        #expect(AttachFrameMeta(payload) == nil, "a meta length past the payload is refused")
    }
}
