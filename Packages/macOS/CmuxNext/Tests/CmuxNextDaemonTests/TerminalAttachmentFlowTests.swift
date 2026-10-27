import Foundation
import Testing
@testable import CmuxNextDaemon

/// The attach decoder reads the key press an `output` event echoes (`flow`, p09-observability-018), so the
/// typing probe's decode mark names that press exactly.
struct TerminalAttachmentFlowTests {
    private let surface = SurfaceID(rawValue: 3)

    private func decode(_ json: String) throws -> DecodedAttachLine {
        let decoded = try TerminalAttachment.decodeAttachLine(name: "output", line: Data(json.utf8), surface: surface,
                                                              decoder: WireCoding.decoder())
        return try #require(decoded)
    }

    @Test func anOutputCarriesTheFlowItEchoes() throws {
        // pid 0x1234 in the high 32 bits, press 7 below: past 2^53, so it must decode exactly.
        let flow: UInt64 = 0x1234 << 32 | 7
        let decoded = try decode(#"{"event":"output","surface":3,"data":"YQ==","flow":\#(flow)}"#)
        #expect(decoded.flow == flow)
        #expect(decoded.event == .output(Data("a".utf8), colors: nil))
    }

    @Test func anOutputWithoutAFlowStillDecodes() throws {
        let decoded = try decode(#"{"event":"output","surface":3,"data":"YQ==","generation":2}"#)
        #expect(decoded.flow == nil)
        #expect(decoded.generation == 2)
        #expect(decoded.event == .output(Data("a".utf8), colors: nil))
    }

    @Test func otherEventsCarryNoFlow() throws {
        let line = Data(#"{"event":"scroll-changed","surface":3,"offset":4,"at_bottom":false}"#.utf8)
        let parsed = try TerminalAttachment.decodeAttachLine(name: "scroll-changed", line: line, surface: surface,
                                                             decoder: WireCoding.decoder())
        let decoded = try #require(parsed)
        #expect(decoded.flow == nil)
    }

    /// A known attach event that does not decode broke the attach contract (p10-stability-178): it throws, naming
    /// the event and never the payload, so the connection reports it as a bad message and closes.
    @Test func aKnownEventThatDoesNotDecodeThrowsUndecodableLine() {
        let line = Data(#"{"event":"output","surface":3,"data":12}"#.utf8)
        #expect(throws: TerminalAttachment.UndecodableLine(event: "output")) {
            try TerminalAttachment.decodeAttachLine(name: "output", line: line, surface: surface, decoder: WireCoding.decoder())
        }
        // An event the view ignores is not a contract break.
        #expect(throws: Never.self) {
            try TerminalAttachment.decodeAttachLine(name: "digest", line: Data("{}".utf8), surface: surface,
                                                    decoder: WireCoding.decoder())
        }
    }

    /// The flow rides behind its chunk to the session (`inputEcho`): the queue merges output before it
    /// into that chunk, never output after it, so the session and the lane mark the chunk that echoes.
    @Test func anEchoMarkerStaysBehindItsChunk() async {
        let queue = TerminalEventQueue(recorder: nil)
        queue.push(.output(Data("a".utf8), colors: nil))
        queue.push(.output(Data("b".utf8), colors: nil))
        queue.pushWithoutWaiting(.inputEcho(flow: 7))
        queue.push(.output(Data("c".utf8), colors: nil))
        #expect(await queue.next() == .output(Data("ab".utf8), colors: nil))
        #expect(await queue.next() == .inputEcho(flow: 7))
        #expect(await queue.next() == .output(Data("c".utf8), colors: nil))
    }
}
