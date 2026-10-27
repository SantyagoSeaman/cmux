//! `cmux_ipc::output_ring` (CMAR layout 1): the header bytes, wrap, lap,
//! an oversized chunk, the bounds a reader checks, and a reader descriptor
//! that refuses every write.

#![cfg(unix)]

mod output_ring {
    use cmux_ipc::output_ring::{
        DEFAULT_CAPACITY, HEADER, MAX_CAPACITY, OutputRingReader, OutputRingWriter, RingError,
        SpanError,
    };
    use cmux_ipc::shm::{self, Mapping, Protection, SegmentKind};
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

    fn pattern(from: u64, len: usize) -> Vec<u8> {
        (from..from + len as u64).map(|offset| b'a' + (offset % 23) as u8).collect()
    }

    fn read(reader: &OutputRingReader, from: u64, to: u64) -> Result<Vec<u8>, SpanError> {
        let mut out = Vec::new();
        reader.read_into(from, to, &mut out).map(|()| out)
    }

    fn header(fd: BorrowedFd<'_>) -> Vec<u8> {
        let view = Mapping::new(fd, HEADER, Protection::ReadOnly).unwrap();
        // SAFETY: the view is HEADER readable bytes.
        unsafe { std::slice::from_raw_parts(view.as_ptr(), HEADER) }.to_vec()
    }

    #[test]
    fn output_ring_header_is_cmar_layout_1() {
        let mut writer = OutputRingWriter::create(0x0102_0304_0506_0708, 4096).unwrap();
        writer.push(b"hello");
        let bytes = header(writer.reader());
        let mut expected = Vec::new();
        expected.extend_from_slice(b"CMAR");
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&4096u64.to_le_bytes());
        expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        expected.extend_from_slice(&(0x0102_0304_0506_0708u64 + 5).to_le_bytes());
        expected.extend_from_slice(&(0x0102_0304_0506_0708u64 + 5).to_le_bytes());
        expected.resize(HEADER, 0);
        assert_eq!(bytes, expected);
        assert_eq!((DEFAULT_CAPACITY, MAX_CAPACITY), (1 << 20, 64 << 20));
    }

    #[test]
    fn output_ring_reads_what_was_published_at_its_offsets() {
        let mut writer = OutputRingWriter::create(1000, 64).unwrap();
        let reader = OutputRingReader::open(writer.reader_fd().unwrap().as_fd()).unwrap();
        assert_eq!((reader.base(), reader.end(), reader.capacity()), (1000, 1000, 64));
        writer.push(b"hello ");
        writer.push(b"world");
        assert_eq!(reader.end(), 1011);
        assert_eq!(read(&reader, 1000, 1011).unwrap(), b"hello world");
        assert_eq!(read(&reader, 1006, 1011).unwrap(), b"world");
        assert_eq!(read(&reader, 1011, 1011).unwrap(), b"");
    }

    #[test]
    fn output_ring_wraps_across_the_end_of_its_data() {
        let mut writer = OutputRingWriter::create(0, 16).unwrap();
        let reader = OutputRingReader::open(writer.reader()).unwrap();
        writer.push(&pattern(0, 12));
        writer.push(&pattern(12, 10));
        // 12..22 sits at data[12..16] and data[0..6].
        assert_eq!(read(&reader, 12, 22).unwrap(), pattern(12, 10));
        assert_eq!(read(&reader, 6, 22).unwrap(), pattern(6, 16));
        // Bytes 0..6 were overwritten by the wrap.
        assert!(matches!(read(&reader, 5, 10), Err(SpanError::Lapped { from: 5, reserve: 22 })));
    }

    /// The reader checks at the cut and after the use: a writer that laps a
    /// span between them is caught by the second check.
    #[test]
    fn output_ring_detects_a_lap_during_a_held_span() {
        let mut writer = OutputRingWriter::create(0, 16).unwrap();
        let reader = OutputRingReader::open(writer.reader()).unwrap();
        writer.push(&pattern(0, 10));
        reader.cut(0, 10).unwrap();
        reader.check(0).unwrap();
        writer.push(&pattern(10, 6));
        reader.check(0).unwrap();
        writer.push(&pattern(16, 1));
        assert_eq!(reader.check(0), Err(SpanError::Lapped { from: 0, reserve: 17 }));
        assert_eq!(reader.check(1), Ok(()));
        // A span longer than the ring is lost however the writer stands.
        assert!(matches!(reader.cut(0, 17), Err(SpanError::Lapped { .. })));
        // A failed read leaves the output as it was.
        let mut out = b"kept".to_vec();
        assert!(reader.read_into(0, 4, &mut out).is_err());
        assert_eq!(out, b"kept");
    }

    #[test]
    fn output_ring_an_oversized_chunk_keeps_its_last_capacity_bytes() {
        let mut writer = OutputRingWriter::create(100, 16).unwrap();
        let reader = OutputRingReader::open(writer.reader()).unwrap();
        writer.push(b"abc");
        writer.push(&pattern(103, 40));
        assert_eq!((writer.end(), reader.end()), (143, 143));
        assert_eq!(read(&reader, 127, 143).unwrap(), pattern(127, 16));
        // Every offset the chunk skipped is lapped, as is the older "abc".
        for from in [100, 103, 120, 126] {
            assert!(
                matches!(read(&reader, from, from + 1), Err(SpanError::Lapped { .. })),
                "{from}"
            );
        }
    }

    #[test]
    fn output_ring_refuses_spans_outside_what_it_held() {
        let mut writer = OutputRingWriter::create(50, 16).unwrap();
        let reader = OutputRingReader::open(writer.reader()).unwrap();
        writer.push(b"0123");
        assert_eq!(reader.cut(49, 52), Err(SpanError::BeforeBase { from: 49, base: 50 }));
        assert_eq!(reader.cut(52, 55), Err(SpanError::NotPublished { to: 55, end: 54 }));
        assert_eq!(reader.cut(53, 52), Err(SpanError::Backwards { from: 53, to: 52 }));
    }

    #[test]
    fn output_ring_capacity_is_bounded() {
        assert!(matches!(
            OutputRingWriter::create(0, 0),
            Err(RingError::BadCapacity { capacity: 0 })
        ));
        assert!(matches!(
            OutputRingWriter::create(0, MAX_CAPACITY + 1),
            Err(RingError::BadCapacity { .. })
        ));
    }

    /// The reader's descriptor cannot change the ring: no writable shared
    /// mapping, no `write`, no resize, no upgrade of a read-only mapping and,
    /// on Linux, no writable reopen through `/proc`.
    #[test]
    fn output_ring_reader_descriptor_refuses_writes() {
        let mut writer = OutputRingWriter::create(0, 4096).unwrap();
        writer.push(b"output");
        let fd = writer.reader_fd().unwrap();
        assert!(!shm::maps_writable(fd.as_fd()));
        // SAFETY: write of 1 byte from a live local to a live descriptor.
        let wrote = unsafe { libc::write(fd.as_raw_fd(), b"x".as_ptr().cast(), 1) };
        assert!(wrote < 0, "write through the reader's descriptor");
        // SAFETY: ftruncate of a live descriptor.
        assert!(unsafe { libc::ftruncate(fd.as_raw_fd(), 0) } != 0, "resize");
        let view = Mapping::new(fd.as_fd(), HEADER + 4096, Protection::ReadOnly).unwrap();
        // SAFETY: changes the protection of a mapping this test owns.
        let upgraded = unsafe {
            libc::mprotect(view.as_ptr().cast(), view.len(), libc::PROT_READ | libc::PROT_WRITE)
        };
        assert!(upgraded != 0, "mprotect made the reader's mapping writable");
        #[cfg(target_os = "linux")]
        {
            let path = std::ffi::CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
            // SAFETY: open with a NUL-terminated path.
            let reopened = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if reopened >= 0 {
                // SAFETY: a fresh descriptor this test owns.
                let reopened = unsafe {
                    <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(reopened)
                };
                assert!(!shm::maps_writable(reopened.as_fd()), "a writable reopen maps writable");
                // SAFETY: as above.
                let wrote = unsafe { libc::write(reopened.as_raw_fd(), b"x".as_ptr().cast(), 1) };
                assert!(wrote < 0, "write through a reopened descriptor");
            }
        }
        // The bytes the writer published are untouched.
        let reader = OutputRingReader::open(fd.as_fd()).unwrap();
        assert_eq!(read(&reader, 0, 6).unwrap(), b"output");
        // The reader's descriptor is close-on-exec.
        // SAFETY: F_GETFD of a live descriptor.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "{}", io::Error::last_os_error());
    }

    /// A reader refuses a descriptor that maps writable, even of a valid ring.
    #[test]
    fn output_ring_reader_refuses_a_writable_descriptor() {
        let segment = shm::create(SegmentKind::OutputRing, HEADER + 64).unwrap();
        let view = Mapping::new(segment.as_fd(), HEADER + 64, Protection::ReadWrite).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"CMAR");
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&64u64.to_le_bytes());
        // SAFETY: the view is HEADER + 64 writable bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), view.as_ptr(), bytes.len()) };
        assert!(matches!(OutputRingReader::open(segment.as_fd()), Err(RingError::Writable)));
    }

    /// A reader refuses a header that is not CMAR layout 1 with a capacity
    /// the segment holds.
    #[test]
    fn output_ring_reader_refuses_a_bad_header() {
        let open = |edit: &dyn Fn(&mut Vec<u8>), size: usize| {
            let segment = shm::create_read_only(SegmentKind::OutputRing, size).unwrap();
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"CMAR");
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&64u64.to_le_bytes());
            bytes.extend_from_slice(&10u64.to_le_bytes());
            bytes.extend_from_slice(&10u64.to_le_bytes());
            edit(&mut bytes);
            // SAFETY: the writable mapping holds `size >= bytes.len()` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    segment.mapping.as_ptr(),
                    bytes.len(),
                );
            }
            OutputRingReader::open(segment.reader.as_fd())
        };
        assert!(open(&|_| {}, HEADER + 64).is_ok());
        assert!(matches!(open(&|b| b[0] = b'X', HEADER + 64), Err(RingError::BadMagic)));
        assert!(matches!(
            open(&|b| b[4] = 2, HEADER + 64),
            Err(RingError::BadLayout { layout: 2 })
        ));
        assert!(matches!(
            open(&|b| b[8..16].copy_from_slice(&0u64.to_le_bytes()), HEADER + 64),
            Err(RingError::BadCapacity { capacity: 0 })
        ));
        assert!(matches!(
            open(&|b| b[8..16].copy_from_slice(&(1u64 << 20).to_le_bytes()), HEADER + 64),
            Err(RingError::TooSmall { .. })
        ));
        assert!(matches!(
            open(&|b| b[24..32].copy_from_slice(&9u64.to_le_bytes()), HEADER + 64),
            Err(RingError::EndBeforeBase { base: 10, end: 9 })
        ));
        // macOS rounds a shm object up to a page: only Linux keeps 32 bytes.
        #[cfg(target_os = "linux")]
        assert!(matches!(open(&|_| {}, 32), Err(RingError::TooSmall { .. })));
    }

    /// The reader's mapping outlives the writer: a daemon that ends the
    /// terminal (or exits) never faults the app.
    #[test]
    fn output_ring_reader_outlives_the_writer() {
        let mut writer = OutputRingWriter::create(0, 64).unwrap();
        writer.push(b"last words");
        let reader = OutputRingReader::open(writer.reader()).unwrap();
        drop(writer);
        assert_eq!(read(&reader, 0, 10).unwrap(), b"last words");
    }

    /// A reader on another thread, while the writer floods: every span that
    /// passes both checks is the bytes written at its offsets.
    #[test]
    fn output_ring_a_concurrent_reader_never_accepts_a_torn_span() {
        const CAPACITY: usize = 4096;
        const TOTAL: u64 = 8 << 20;
        let mut writer = OutputRingWriter::create(0, CAPACITY).unwrap();
        let reader = OutputRingReader::open(writer.reader()).unwrap();
        let checker = std::thread::spawn(move || {
            let (mut accepted, mut lapped, mut cursor) = (0u64, 0u64, 0u64);
            let mut out = Vec::new();
            while cursor < TOTAL {
                let end = reader.end();
                let to = end.min(cursor + 1500);
                if to == cursor {
                    std::hint::spin_loop();
                    continue;
                }
                out.clear();
                match reader.read_into(cursor, to, &mut out) {
                    Ok(()) => {
                        assert_eq!(out, pattern(cursor, (to - cursor) as usize), "{cursor}..{to}");
                        accepted += to - cursor;
                        cursor = to;
                    }
                    Err(SpanError::Lapped { .. }) => {
                        lapped += 1;
                        cursor = reader.end();
                    }
                    Err(error) => panic!("{error}"),
                }
            }
            (accepted, lapped)
        });
        let mut offset = 0;
        while offset < TOTAL {
            let len = 1 + (offset % 977) as usize;
            writer.push(&pattern(offset, len));
            offset += len as u64;
        }
        let (accepted, _lapped) = checker.join().unwrap();
        assert!(accepted > 0);
    }
}
