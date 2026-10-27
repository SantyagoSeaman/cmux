import Darwin
import Foundation

/// The daemon's side of a real ring for the ring stream tests (p05-ipc-119): a CMAR file written through a
/// read-write mapping as cmux-ipc output_ring.rs `OutputRingWriter::push` writes it, read by `SharedOutputRing`
/// through its own `PROT_READ` mapping (a regular file opened `O_RDONLY` refuses `PROT_WRITE, MAP_SHARED` like the
/// daemon's read-only descriptor). The path is unlinked once both descriptors are open. One thread writes.
final class DaemonRingFile {
    let capacity: UInt64
    private let writable: Int32
    private let readable: Int32
    private let mapping: UnsafeMutableRawPointer
    private let mappedByteCount: Int
    /// Offset after the last published byte.
    private(set) var publishedEnd: UInt64 = 0

    init(capacity: Int) throws {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("cmar-\(UUID().uuidString)").path
        writable = open(path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0o600)
        guard writable >= 0 else { throw POSIXError(.EIO) }
        readable = open(path, O_RDONLY | O_CLOEXEC)
        unlink(path)
        guard readable >= 0 else { throw POSIXError(.EIO) }
        mappedByteCount = 64 + capacity
        guard ftruncate(writable, off_t(mappedByteCount)) == 0 else { throw POSIXError(.EIO) }
        // SAFETY: a read-write mapping of the file's 64 + capacity bytes; deinit unmaps it.
        guard let mapping = unsafe mmap(nil, mappedByteCount, PROT_READ | PROT_WRITE, MAP_SHARED, writable, 0),
              unsafe mapping != UnsafeMutableRawPointer(bitPattern: -1) else { throw POSIXError(.EIO) }
        self.mapping = unsafe mapping
        self.capacity = UInt64(capacity)
        // SAFETY (header writes): offsets below 64, inside the mapping, aligned for their types. Base, end and
        // reserve stay 0 (ftruncate zeroed them).
        for (index, byte) in Array("CMAR".utf8).enumerated() {
            unsafe mapping.storeBytes(of: byte, toByteOffset: index, as: UInt8.self)
        }
        unsafe mapping.storeBytes(of: UInt32(1).littleEndian, toByteOffset: 4, as: UInt32.self)
        unsafe mapping.storeBytes(of: UInt64(capacity).littleEndian, toByteOffset: 8, as: UInt64.self)
    }

    deinit {
        // SAFETY: the mapping of init, unmapped once.
        unsafe munmap(mapping, mappedByteCount)
        close(writable)
        close(readable)
    }

    /// A copy of the read-only descriptor; the ring takes it.
    func readOnlyDescriptor() -> Int32 { dup(readable) }

    /// The daemon's push with one consumer cursor acked at `cursor`: a chunk that could overwrite a published byte
    /// at or after the cursor is skipped instead, the base moving past it before the end does. Whether it skipped.
    func push(_ bytes: [UInt8], cursor: UInt64) -> Bool {
        let newEnd = publishedEnd + UInt64(bytes.count)
        guard cursor < publishedEnd, newEnd > cursor + capacity else {
            push(bytes)
            return false
        }
        // SAFETY: header words below 64, inside the mapping.
        unsafe mapping.storeBytes(of: newEnd.littleEndian, toByteOffset: 16, as: UInt64.self)
        unsafe mapping.storeBytes(of: newEnd.littleEndian, toByteOffset: 24, as: UInt64.self)
        publishedEnd = newEnd
        return true
    }

    /// Appends and publishes `bytes`: reserve, the copy (split at the wrap), then end.
    func push(_ bytes: [UInt8]) {
        let newEnd = publishedEnd + UInt64(bytes.count)
        let kept = bytes.suffix(Int(capacity))
        let start = newEnd - UInt64(kept.count)
        // SAFETY: header words below 64; data writes at 64 + (offset % capacity), inside the mapping.
        unsafe mapping.storeBytes(of: newEnd.littleEndian, toByteOffset: 32, as: UInt64.self)
        for (index, byte) in kept.enumerated() {
            let offset = 64 + Int((start + UInt64(index)) % capacity)
            unsafe mapping.storeBytes(of: byte, toByteOffset: offset, as: UInt8.self)
        }
        unsafe mapping.storeBytes(of: newEnd.littleEndian, toByteOffset: 24, as: UInt64.self)
        publishedEnd = newEnd
    }
}
