import CmuxNextIPC
import CmuxNextSharedRegionC
import Darwin
import Foundation
import Testing

/// The app's SHARED_BUFFER pool (p05-ipc-115): the frame bytes cmux-ipc shared_buffer.rs reads, one frame and one
/// read-only shm descriptor for a 10 MB paste, a reused region with no second descriptor, the keep budget both
/// ends apply, the daemon's copy unchanged by a later write, and no descriptor left open.
@Suite nonisolated struct SharedBufferTests {
    private func body(_ count: Int, seed: UInt8) -> Data {
        Data((0 ..< count).map { UInt8(truncatingIfNeeded: $0 &* 31) &+ seed })
    }

    private func stage(_ pool: inout SharedBufferPool, _ body: Data, request: UInt32) -> SharedBufferPool.Staged? {
        pool.stage(body, requestID: request, methodID: 0x0101, priority: 1)
    }

    /// The first `count` bytes of the region `fd` passes, as the daemon reads them: a read-only mapping, copied.
    private func received(_ fd: Int32, count: Int) -> Data? {
        guard let mapping = unsafe mmap(nil, count, PROT_READ, MAP_SHARED, fd, 0),
              unsafe mapping != UnsafeMutableRawPointer(bitPattern: -1) else { return nil }
        defer { unsafe munmap(mapping, count) }
        return unsafe Data(bytes: mapping, count: count)
    }

    private func openDescriptors() -> Int {
        (Int32(0) ..< 4096).count(where: { fcntl($0, F_GETFD) >= 0 })
    }

    @Test func theFrameIsTheLayoutTheDaemonReads() {
        // shared_buffer_tests.rs `shared_buffer_descriptor_bytes_are_the_documented_layout`.
        let descriptor = SharedBufferDescriptor(requestID: 0x0403_0201, methodID: 0x0605, priority: 2, callFlags: 0x04,
                                                bodyLength: 0x0A09_0807, region: 3, attached: true)
        let frame = [UInt8](descriptor.frame(regionBytes: 1 << 20))
        #expect(frame.count == 32)
        #expect(Array(frame[0 ..< 4]) == [0xFF, 10, 0, 0])
        #expect(Array(frame[4 ..< 8]) == [16, 0, 0, 0])
        #expect(Array(frame[8 ..< 16]) == [0, 0, 0x10, 0, 0, 0, 0, 0])
        #expect(Array(frame[16...]) == [1, 2, 3, 4, 5, 6, 2, 0x04, 7, 8, 9, 10, 3, 0, 1, 1])
    }

    @Test func regionSizesAndTheKeepRuleMatchTheDaemon() {
        #expect(SharedBufferPool.regionBytes(for: SharedBufferPool.inlineMax + 1) == 256 << 10)
        #expect(SharedBufferPool.regionBytes(for: 10_000_000) == 16 << 20)
        #expect(SharedBufferPool.regionBytes(for: 32 << 20) == 32 << 20)
        #expect(SharedBufferPool.regionBytes(for: (32 << 20) + 1) == nil)
        #expect(SharedBufferPool.keeps(0, SharedBufferPool.keepBytes))
        #expect(!SharedBufferPool.keeps(1, SharedBufferPool.keepBytes))
    }

    @Test func aTenMBPasteIsOneFrameAndOneReadOnlyShmDescriptor() throws {
        var pool = SharedBufferPool()
        let paste = body(10 << 20, seed: 7)
        let staged = try #require(stage(&pool, paste, request: 1))
        #expect(staged.frame.count == 32, "one frame, no chunk of the body")
        let fd = try #require(staged.fd)
        defer { Darwin.close(fd) }
        #expect(cmux_shared_region_is_sealed(fd) == 1, "a read-only shm object, which nobody can shrink")
        #expect((fcntl(fd, F_GETFD) & FD_CLOEXEC) != 0)
        #expect(received(fd, count: paste.count) == paste)
    }

    @Test func aWriteAfterTheDaemonsCopyDoesNotChangeIt() throws {
        var pool = SharedBufferPool()
        let first = body(SharedBufferPool.inlineMax * 3, seed: 1)
        let staged = try #require(stage(&pool, first, request: 1))
        let fd = try #require(staged.fd)
        defer { Darwin.close(fd) }
        let copy = try #require(received(fd, count: first.count))
        #expect(pool.overwrite(region: staged.region, with: body(first.count, seed: 99)))
        #expect(copy == first, "the daemon's bytes are its own copy")
        #expect(received(fd, count: first.count) != first, "the region itself did change")
    }

    @Test func aReusedRegionPassesNoSecondDescriptor() throws {
        var pool = SharedBufferPool()
        let first = try #require(stage(&pool, body(SharedBufferPool.inlineMax * 4, seed: 1), request: 1))
        defer { if let fd = first.fd { Darwin.close(fd) } }
        // Busy until its reply: a second body takes another region.
        let second = try #require(stage(&pool, body(SharedBufferPool.inlineMax * 2, seed: 2), request: 2))
        defer { if let fd = second.fd { Darwin.close(fd) } }
        #expect(second.region == 1 && second.fd != nil)
        #expect(pool.release(requestID: 1))
        #expect(!pool.release(requestID: 1), "a request frees its region once")
        let third = try #require(stage(&pool, body(SharedBufferPool.inlineMax * 4 - 5, seed: 3), request: 3))
        #expect(third.region == 0 && third.fd == nil)
        #expect(pool.regionCount == 2)
    }

    @Test func smallBodiesAndABusyPoolStayInline() {
        var pool = SharedBufferPool()
        #expect(stage(&pool, body(SharedBufferPool.inlineMax, seed: 0), request: 1) == nil)
        var passed: [Int32] = []
        for request in 0 ..< UInt32(SharedBufferPool.maxRegions) {
            let staged = stage(&pool, body(SharedBufferPool.inlineMax + 1, seed: 0), request: request)
            #expect(staged != nil)
            passed += staged?.fd.map { [$0] } ?? []
        }
        #expect(stage(&pool, body(SharedBufferPool.inlineMax + 1, seed: 0), request: 9) == nil)
        pool.release(requestID: 2)
        #expect(stage(&pool, body(SharedBufferPool.inlineMax + 1, seed: 0), request: 9)?.region == 2)
        passed.forEach { Darwin.close($0) }
    }

    @Test func aRegionPastTheKeepBudgetIsAnnouncedAgain() throws {
        var pool = SharedBufferPool()
        let big = body(20 << 20, seed: 4)
        let first = try #require(stage(&pool, big, request: 1))
        if let fd = first.fd { Darwin.close(fd) }
        pool.release(requestID: 1)
        #expect(pool.regionCount == 0, "the daemon mapped a 32 MiB region for one body only")
        let again = try #require(stage(&pool, big, request: 2))
        #expect(again.fd != nil)
        if let fd = again.fd { Darwin.close(fd) }
    }

    @Test func noDescriptorStaysOpen() throws {
        let before = openDescriptors()
        do {
            var pool = SharedBufferPool()
            for (request, count) in [(1, 300_000), (2, 9_000_000), (3, 300_000), (4, 20_000_000)] {
                let staged = try #require(stage(&pool, body(count, seed: UInt8(request)), request: UInt32(request)))
                if let fd = staged.fd { Darwin.close(fd) }
                pool.release(requestID: UInt32(request))
            }
            // 512 KiB stays; the 16 MiB and 32 MiB regions passed the keep budget and were dropped.
            #expect(pool.regionCount == 1)
        }
        #expect(openDescriptors() == before, "a region's descriptor stayed open")
    }
}

