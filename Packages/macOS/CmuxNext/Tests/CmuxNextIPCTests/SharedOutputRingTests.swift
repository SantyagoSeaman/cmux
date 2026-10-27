import CmuxNextIPC
import Darwin
import Foundation
import Testing

/// ``SharedOutputRing`` (plans/cmux-next/chrome-parity/p05-ipc.md, task 112): a writable descriptor is refused,
/// spans point into the read-only mapping, a lap during a held span marks the ring lapped, and the last
/// release unmaps.
@Suite(.timeLimit(.minutes(1))) struct SharedOutputRingTests {
    private static let capacity: UInt64 = 4096

    @Test func aWritableDescriptorIsRefused() throws {
        let writer = try RingFile()
        #expect(throws: SharedOutputRing.OpenError.writable) { try SharedOutputRing(adopting: writer.writableDescriptor()) }
        // The read-only descriptor of the same ring opens.
        let ring = try SharedOutputRing(adopting: writer.readOnlyDescriptor())
        #expect(ring.capacity == Self.capacity)
    }

    @Test func aBadHeaderIsAProtocolError() throws {
        let cases: [(RingFile.Header, SharedOutputRing.OpenError)] = [
            (RingFile.Header(magic: Array("CMAX".utf8)), .badMagic),
            (RingFile.Header(layout: 2), .badLayout(2)),
            (RingFile.Header(capacity: 0), .badCapacity(0)),
            (RingFile.Header(capacity: SharedOutputRing.maxCapacity + 1), .badCapacity(SharedOutputRing.maxCapacity + 1)),
            (RingFile.Header(base: 10, end: 9), .endBeforeBase(base: 10, end: 9)),
            (RingFile.Header(fileSize: 64 + 100), .tooSmall(size: 164, needed: 64 + Self.capacity)),
            (RingFile.Header(fileSize: 10), .tooSmall(size: 10, needed: 64)),
        ]
        for (header, error) in cases {
            let file = try RingFile(header)
            #expect(throws: error) { try SharedOutputRing(adopting: file.readOnlyDescriptor()) }
        }
    }

    @Test func spansPointIntoTheMappingAndSplitAtTheWrap() throws {
        let file = try RingFile()
        let ring = try SharedOutputRing(adopting: file.readOnlyDescriptor())
        let first = Self.bytes(4000, seed: 1)
        file.push(first)
        do {
            let whole = try ring.cut(from: 0, to: 4000)
            #expect(whole.head == Data(first))
            #expect(whole.tail == nil)
            #expect(try Self.address(of: #require(whole.head)) == ring.dataAddresses.lowerBound)
        }

        let second = Self.bytes(200, seed: 2)
        file.push(second)
        let wrapped = try ring.cut(from: 4000, to: 4200)
        let head = try #require(wrapped.head)
        let tail = try #require(wrapped.tail)
        #expect(head == Data(second.prefix(96)))
        #expect(tail == Data(second.suffix(104)))
        #expect(Self.address(of: head) == ring.dataAddresses.lowerBound + 4000)
        #expect(Self.address(of: tail) == ring.dataAddresses.lowerBound)
        #expect(ring.dataAddresses.count == Int(Self.capacity))

        let empty = try ring.cut(from: 4200, to: 4200)
        #expect(empty.head == nil && empty.tail == nil)
        #expect(!ring.takeLapped())
        #expect(ring.cutLapCount == 0 && ring.releaseLapCount == 0)
    }

    @Test func aRangeTheRingCannotServeIsRefusedAtTheCut() throws {
        let file = try RingFile(RingFile.Header(base: 1000, end: 1000))
        let ring = try SharedOutputRing(adopting: file.readOnlyDescriptor())
        file.push(Self.bytes(100, seed: 3))
        #expect(throws: SharedOutputRing.SpanError.backwards(from: 1050, to: 1040)) { try ring.cut(from: 1050, to: 1040) }
        #expect(throws: SharedOutputRing.SpanError.beforeBase(from: 999, base: 1000)) { try ring.cut(from: 999, to: 1010) }
        #expect(throws: SharedOutputRing.SpanError.notPublished(to: 1101, end: 1100)) { try ring.cut(from: 1000, to: 1101) }
        #expect(ring.cutLapCount == 0)

        // The writer goes a capacity past 1000: [1000, 1100) is being overwritten, nothing is cut.
        file.push(Self.bytes(Int(Self.capacity), seed: 4))
        let reserve = 1100 + Self.capacity
        #expect(throws: SharedOutputRing.SpanError.lapped(from: 1000, reserve: reserve)) { try ring.cut(from: 1000, to: 1100) }
        // A range longer than the ring is lapped whatever the reserve.
        #expect(throws: SharedOutputRing.SpanError.lapped(from: 1050, reserve: reserve)) { try ring.cut(from: 1050, to: reserve) }
        #expect(ring.cutLapCount == 2)
        #expect(!ring.takeLapped())
    }

    @Test func aLapWhileASpanIsHeldMarksTheRingLapped() throws {
        let file = try RingFile()
        let ring = try SharedOutputRing(adopting: file.readOnlyDescriptor())
        file.push(Self.bytes(100, seed: 5))
        do {
            let held = try #require(ring.cut(from: 0, to: 100).head)
            // The writer reaches exactly a capacity past the span's start: the span is still whole.
            file.push(Self.bytes(Int(Self.capacity) - 100, seed: 6))
            do { _ = try ring.cut(from: 100, to: 200) } // cut and released at once, not lapped
            #expect(!ring.takeLapped())

            // One more byte overwrites the span's first byte while it is held (Ghostty parsing it).
            withExtendedLifetime(held) {
                file.push(Self.bytes(1, seed: 7))
                #expect(!ring.takeLapped()) // only the release checks
            }
        }
        // The held span was released at the end of its scope.
        #expect(ring.releaseLapCount == 1)
        #expect(ring.takeLapped())
        #expect(!ring.takeLapped()) // taken once
        #expect(ring.cutLapCount == 0)
    }

    @Test func theLastReleaseUnmaps() throws {
        let file = try RingFile()
        var ring: SharedOutputRing? = try SharedOutputRing(adopting: file.readOnlyDescriptor())
        weak var watched = ring
        let bytes = Self.bytes(300, seed: 8)
        file.push(bytes)
        var spans: SharedOutputRing.Spans? = try ring?.cut(from: 0, to: 300)

        // The stream lets go (detach, stream end, connection loss): the held span keeps the mapping.
        ring = nil
        #expect(watched != nil)
        #expect(spans?.head == Data(bytes))

        // Ghostty drops the last span: the ring unmaps.
        spans = nil
        #expect(watched == nil)
    }

    private static func bytes(_ count: Int, seed: UInt8) -> [UInt8] {
        (0 ..< count).map { UInt8(truncatingIfNeeded: $0 &* 31 &+ Int(seed)) }
    }

    private static func address(of span: Data) -> UInt {
        span.withUnsafeBytes { unsafe UInt(bitPattern: $0.baseAddress) }
    }
}

/// The daemon's side of a ring for the tests: a CMAR file the test writes through a read-write mapping, as
/// cmux-ipc output_ring.rs `OutputRingWriter::push` does. The ring reads the same pages through its own
/// `PROT_READ` mapping (a regular file opened `O_RDONLY` refuses `PROT_WRITE, MAP_SHARED` like the daemon's
/// read-only shm descriptor). The path is unlinked once both descriptors are open.
nonisolated private final class RingFile {
    struct Header {
        var magic = Array("CMAR".utf8)
        var layout: UInt32 = 1
        var capacity: UInt64 = 4096
        var base: UInt64 = 0
        var end: UInt64 = 0
        /// The object's size; nil for header plus capacity.
        var fileSize: Int?
    }

    private let writable: Int32
    private let readable: Int32
    private let mapping: UnsafeMutableRawPointer
    private let mappedByteCount: Int
    private let capacity: UInt64
    private var end: UInt64

    init(_ header: Header = Header()) throws {
        let path = FileManager.default.temporaryDirectory
            .appendingPathComponent("cmar-\(UUID().uuidString)").path
        writable = open(path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0o600)
        guard writable >= 0 else { throw POSIXError(.EIO) }
        readable = open(path, O_RDONLY | O_CLOEXEC)
        unlink(path)
        guard readable >= 0 else { throw POSIXError(.EIO) }
        let fileSize = header.fileSize ?? 64 + Int(min(header.capacity, SharedOutputRing.maxCapacity))
        guard ftruncate(writable, off_t(fileSize)) == 0 else { throw POSIXError(.EIO) }
        mappedByteCount = max(fileSize, 64)
        // SAFETY: a read-write mapping of the file's pages (past a short file's end only the header is written,
        // inside its first page); deinit unmaps it.
        guard let mapping = unsafe mmap(nil, mappedByteCount, PROT_READ | PROT_WRITE, MAP_SHARED, writable, 0),
              unsafe mapping != UnsafeMutableRawPointer(bitPattern: -1) else { throw POSIXError(.EIO) }
        self.mapping = unsafe mapping
        capacity = header.capacity
        end = header.end
        guard fileSize >= 64 else { return }
        // SAFETY (header writes): offsets below 64, inside the mapping, aligned for their types.
        for (index, byte) in header.magic.enumerated() {
            unsafe mapping.storeBytes(of: byte, toByteOffset: index, as: UInt8.self)
        }
        unsafe mapping.storeBytes(of: header.layout.littleEndian, toByteOffset: 4, as: UInt32.self)
        unsafe mapping.storeBytes(of: header.capacity.littleEndian, toByteOffset: 8, as: UInt64.self)
        unsafe mapping.storeBytes(of: header.base.littleEndian, toByteOffset: 16, as: UInt64.self)
        unsafe mapping.storeBytes(of: header.end.littleEndian, toByteOffset: 24, as: UInt64.self)
        unsafe mapping.storeBytes(of: header.end.littleEndian, toByteOffset: 32, as: UInt64.self)
    }

    deinit {
        // SAFETY: the mapping of init, unmapped once.
        unsafe munmap(mapping, mappedByteCount)
        close(writable)
        close(readable)
    }

    /// A copy of the read-only descriptor; the ring takes it.
    func readOnlyDescriptor() -> Int32 { dup(readable) }

    /// A copy of the read-write descriptor; the ring takes it.
    func writableDescriptor() -> Int32 { dup(writable) }

    /// Appends and publishes `bytes`: reserve, the copy (split at the wrap), then end. The test thread is the
    /// ring's only reader here, so plain stores in this order are what the reader sees.
    func push(_ bytes: [UInt8]) {
        let newEnd = end + UInt64(bytes.count)
        let kept = bytes.suffix(Int(capacity))
        let start = newEnd - UInt64(kept.count)
        // SAFETY: header words below 64; data writes at 64 + (offset % capacity) for at most capacity bytes, inside
        // the mapping of 64 + capacity bytes.
        unsafe mapping.storeBytes(of: newEnd.littleEndian, toByteOffset: 32, as: UInt64.self)
        for (index, byte) in kept.enumerated() {
            let offset = 64 + Int((start + UInt64(index)) % capacity)
            unsafe mapping.storeBytes(of: byte, toByteOffset: offset, as: UInt8.self)
        }
        unsafe mapping.storeBytes(of: newEnd.littleEndian, toByteOffset: 24, as: UInt64.self)
        end = newEnd
    }
}
