import CmuxNextSharedRegionC
import Darwin
public import Foundation

/// The SHARED_BUFFER frame's payload (p05-ipc-115; cmux-ipc shared_buffer.rs, whose module docs give the
/// layout): a request whose body is in one of the connection's pooled shared regions.
///
/// ```text
/// bytes  0..8   the request's call fields (request id u32, method id u16, priority u8, flags u8)
/// bytes  8..12  body length u32
/// bytes 12..14  region id u16 (< SharedBufferPool.maxRegions)
/// byte  14      layout version (1)
/// byte  15      flags: 0x01 ATTACHED, the frame passes the region's read-only fd with its first byte
/// ```
public struct SharedBufferDescriptor: Sendable, Equatable {
    /// SHARED_BUFFER (cmux-ipc `FrameKind`).
    public static let frameKind: UInt8 = 10
    /// The payload's length, and the layout version this build writes and reads.
    public static let byteCount = 16
    public static let layout: UInt8 = 1

    public var requestID: UInt32
    public var methodID: UInt16
    public var priority: UInt8
    public var callFlags: UInt8
    public var bodyLength: UInt32
    public var region: UInt16
    /// The frame passes the region's descriptor; the daemon replaces the region its id named before.
    public var attached: Bool

    public init(requestID: UInt32, methodID: UInt16, priority: UInt8, callFlags: UInt8 = 0, bodyLength: UInt32,
                region: UInt16, attached: Bool) {
        self.requestID = requestID
        self.methodID = methodID
        self.priority = priority
        self.callFlags = callFlags
        self.bodyLength = bodyLength
        self.region = region
        self.attached = attached
    }

    /// The whole frame on stream 0: the 16-byte header (kind SHARED_BUFFER, aux the region's byte size), then
    /// the descriptor.
    public func frame(regionBytes: UInt64) -> Data {
        var frame = Data(capacity: FrameHeader.byteCount + Self.byteCount)
        frame.append(FrameHeader.magic)
        frame.append(Self.frameKind)
        Self.append(UInt16.zero, to: &frame)
        Self.append(UInt32(Self.byteCount), to: &frame)
        Self.append(regionBytes, to: &frame)
        Self.append(requestID, to: &frame)
        Self.append(methodID, to: &frame)
        frame.append(priority)
        frame.append(callFlags)
        Self.append(bodyLength, to: &frame)
        Self.append(region, to: &frame)
        frame.append(Self.layout)
        frame.append(attached ? 0x01 : 0x00)
        return frame
    }

    private static func append<Value: FixedWidthInteger & UnsignedInteger>(_ value: Value, to data: inout Data) {
        for shift in stride(from: 0, to: Value.bitWidth, by: 8) {
            data.append(UInt8(truncatingIfNeeded: value >> shift))
        }
    }
}

/// One region of a ``SharedBufferPool``: an unlinked POSIX shm object of ``byteCount`` bytes
/// (`cmux_shared_buffer_region_create`), mapped read-write here, and the read-only descriptor a frame passes.
/// xnu sizes a POSIX shm object once, so nobody can shrink it under the daemon's mapping; the daemon maps only a
/// read-only descriptor whose `fstat` reports no file type, as a shm object's does (shared_buffer.rs `fixed_size`). Unmapped and closed when the last reference goes.
public final class SharedBufferRegion {
    public let byteCount: Int
    private let base: UnsafeMutableRawPointer
    private let readOnlyFD: Int32

    /// A zeroed region of `byteCount` bytes, or nil when the system refuses one.
    public init?(byteCount: Int) {
        guard byteCount > 0 else { return nil }
        var readOnly: Int32 = -1
        let writable = unsafe cmux_shared_buffer_region_create(byteCount, &readOnly)
        guard writable >= 0 else { return nil }
        // The mapping keeps the object: only the read-only descriptor stays open.
        defer { Darwin.close(writable) }
        // SAFETY: maps the `byteCount` bytes the object was just sized to, read-write and shared.
        guard let mapping = unsafe mmap(nil, byteCount, PROT_READ | PROT_WRITE, MAP_SHARED, writable, 0),
              unsafe mapping != UnsafeMutableRawPointer(bitPattern: -1) else {
            Darwin.close(readOnly)
            return nil
        }
        self.byteCount = byteCount
        base = unsafe mapping
        readOnlyFD = readOnly
    }

    deinit {
        // SAFETY: unmaps the mapping init made, once.
        unsafe munmap(base, byteCount)
        Darwin.close(readOnlyFD)
    }

    /// Writes `bytes` at the region's start; false when they do not fit.
    public func write(_ bytes: Data) -> Bool {
        guard bytes.count <= byteCount else { return false }
        // SAFETY: the mapping is writable for `byteCount >= bytes.count` bytes and does not overlap `bytes`.
        unsafe bytes.withUnsafeBytes { source in
            guard let start = source.baseAddress else { return }
            unsafe base.copyMemory(from: start, byteCount: source.count)
        }
        return true
    }

    /// A close-on-exec duplicate of the read-only descriptor, for a frame to pass (the frame writer closes it
    /// once sent); nil when none can be made.
    public func passableDescriptor() -> Int32? {
        let fd = fcntl(readOnlyFD, F_DUPFD_CLOEXEC, 0)
        return fd >= 0 ? fd : nil
    }
}

/// The app's end of SHARED_BUFFER (p05-ipc-115; cmux-ipc shared_buffer.rs `SharedBufferPool`, the same rules).
///
/// A request body above ``inlineMax`` goes in a pooled region instead of inline in its REQUEST frame, as Mojo's
/// `BigBuffer` moves a payload past `kMaxInlineBytes` into shared memory (mojo/public/cpp/base/big_buffer.h). The
/// daemon copies the body out before it parses it, so a later write here changes nothing it validated. A region
/// stays mapped on both ends while the kept regions hold at most ``keepBytes`` (both ends apply the same
/// ``keeps(_:_:)`` to the same frames): a reused region passes no descriptor, so a body costs one frame and one
/// copy on each side. A region holds one request's body from ``stage(_:requestID:methodID:priority:)`` until that
/// request's reply (``release(requestID:)``); with every region busy, a body goes inline.
///
/// Owned by one connection's socket lock: nothing here locks.
public struct SharedBufferPool {
    /// The largest body sent inline: Mojo's `BigBuffer` `kMaxInlineBytes`.
    public static let inlineMax = 64 * 1024
    public static let maxRegions = 4
    public static let minRegionBytes = 256 * 1024
    /// One frame's payload bound (cmux-ipc `MAX_FRAME_PAYLOAD`).
    public static let maxRegionBytes = 32 * 1024 * 1024
    /// What the kept regions of one connection hold together.
    public static let keepBytes = 16 * 1024 * 1024

    /// The region size a body of `count` bytes goes in: a power of two, at least ``minRegionBytes``; nil past
    /// ``maxRegionBytes``.
    public static func regionBytes(for count: Int) -> Int? {
        guard count >= 0, count <= maxRegionBytes else { return nil }
        guard count > minRegionBytes else { return minRegionBytes }
        return 1 << (Int.bitWidth - (count - 1).leadingZeroBitCount)
    }

    /// Whether a region of `bytes` stays mapped after its body, with `others` bytes kept already.
    public static func keeps(_ others: Int, _ bytes: Int) -> Bool {
        others + bytes <= keepBytes
    }

    /// What to write for one staged body: the SHARED_BUFFER frame and, when it attaches its region, a
    /// descriptor the caller owns and passes with the frame's first byte.
    public struct Staged {
        public let frame: Data
        public let fd: Int32?
        public let region: UInt16
    }

    private struct Slot {
        let region: SharedBufferRegion
        /// A frame attached this region's descriptor.
        var announced = false
        /// The daemon keeps it mapped after the announcing body.
        var kept = false
        /// The request whose body it holds until its reply.
        var busy: UInt32?
    }

    private var slots = [Slot?](repeating: nil, count: SharedBufferPool.maxRegions)

    public init() {}

    /// Writes `body` into a free region and returns its frame for request `requestID`; nil sends it inline: a
    /// body of at most ``inlineMax`` or above ``maxRegionBytes``, every region busy, or no region to be had.
    public mutating func stage(_ body: Data, requestID: UInt32, methodID: UInt16, priority: UInt8) -> Staged? {
        guard body.count > Self.inlineMax, let need = Self.regionBytes(for: body.count),
              let index = choose(body.count) else { return nil }
        if (slots[index]?.region.byteCount ?? 0) < body.count {
            guard let region = SharedBufferRegion(byteCount: need) else { return nil }
            slots[index] = Slot(region: region)
        }
        guard var slot = slots[index], slot.region.write(body) else { return nil }
        let attached = !slot.announced
        var fd: Int32?
        if attached {
            guard let passed = slot.region.passableDescriptor() else { return nil }
            fd = passed
            slot.announced = true
            slot.kept = Self.keeps(keptBytes(except: index), slot.region.byteCount)
        }
        slot.busy = requestID
        slots[index] = slot
        let region = UInt16(index)
        let descriptor = SharedBufferDescriptor(requestID: requestID, methodID: methodID, priority: priority,
                                                bodyLength: UInt32(body.count), region: region, attached: attached)
        return Staged(frame: descriptor.frame(regionBytes: UInt64(slot.region.byteCount)), fd: fd, region: region)
    }

    /// Frees the region of request `requestID` after its reply; one the daemon mapped for that body only is
    /// dropped, so the next body announces a new region. False when no region held that request's body.
    @discardableResult
    public mutating func release(requestID: UInt32) -> Bool {
        guard let index = slots.firstIndex(where: { $0?.busy == requestID }) else { return false }
        if slots[index]?.kept == true {
            slots[index]?.busy = nil
        } else {
            slots[index] = nil
        }
        return true
    }

    /// Regions the pool holds now.
    public var regionCount: Int { slots.count(where: { $0 != nil }) }

    /// Writes `bytes` into region `region` whatever it holds: a test of what the daemon sees when the app
    /// writes a region after sending it.
    public func overwrite(region: UInt16, with bytes: Data) -> Bool {
        guard Int(region) < slots.count, let slot = slots[Int(region)] else { return false }
        return slot.region.write(bytes)
    }

    /// The free region that fits `count` best; else an empty id; else the smallest free region, to replace.
    private func choose(_ count: Int) -> Int? {
        let free = slots.indices.filter { slots[$0] != nil && slots[$0]?.busy == nil }
        let size = { (index: Int) in slots[index]?.region.byteCount ?? 0 }
        if let fit = free.filter({ size($0) >= count }).min(by: { size($0) < size($1) }) { return fit }
        if let empty = slots.firstIndex(where: { $0 == nil }) { return empty }
        return free.min(by: { size($0) < size($1) })
    }

    private func keptBytes(except index: Int) -> Int {
        slots.indices.reduce(0) { total, at in
            guard at != index, let slot = slots[at], slot.announced, slot.kept else { return total }
            return total + slot.region.byteCount
        }
    }
}
