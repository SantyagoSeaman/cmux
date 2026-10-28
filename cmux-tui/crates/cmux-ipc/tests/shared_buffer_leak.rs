//! `cmux_ipc::shared_buffer` leaks no descriptor and no mapping: regions
//! staged, sent, received, reused, mapped for one body and refused all close
//! and unmap when their owners drop. One test in its own binary, so no other
//! test opens or closes descriptors while it counts.

#![cfg(unix)]

mod cmux_ipc_shared_buffer {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    use cmux_ipc::fds::{self, FdReceiver};
    use cmux_ipc::shared_buffer::{
        SharedBufferDescriptor, SharedBufferMappings, SharedBufferPool, SharedBufferRefusal,
    };
    use cmux_ipc::{Aux, CallAux, CallFlags, FrameReader, Message, PriorityHint, ReaderLimits};

    fn open_descriptors() -> usize {
        // SAFETY: F_GETFD only asks whether a descriptor number is open.
        (0..4096).filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0).count()
    }

    /// Mappings of shared buffer regions (Linux names its memfds in maps).
    fn region_mappings() -> usize {
        std::fs::read_to_string("/proc/self/maps").map_or(0, |maps| {
            maps.lines().filter(|line| line.contains("cmux-shared-buffer")).count()
        })
    }

    fn call(request_id: u32) -> CallAux {
        CallAux {
            request_id,
            method_id: 1,
            priority: PriorityHint::Control,
            flags: CallFlags::empty(),
        }
    }

    fn receive(
        receiver: &UnixStream,
        reader: &mut FrameReader,
        fds: &mut FdReceiver,
        mappings: &mut SharedBufferMappings,
    ) -> Result<Vec<u8>, SharedBufferRefusal> {
        loop {
            if let Some(Message::Frame(frame)) = reader.next_message().unwrap() {
                let Aux::SharedBufferSize(bytes) = frame.header().aux() else { panic!("aux") };
                let descriptor = SharedBufferDescriptor::decode(frame.payload())?;
                let fd = if descriptor.attached { fds.take(1).unwrap().pop() } else { None };
                return mappings.receive(bytes, &descriptor, fd);
            }
            reader.read_with(|room| fds.recv_into(receiver.as_raw_fd(), room, |_| {})).unwrap();
        }
    }

    #[test]
    fn no_descriptor_or_mapping_leaks() {
        let before = open_descriptors();
        {
            let (sender, receiver) = UnixStream::pair().unwrap();
            let mut reader = FrameReader::new(ReaderLimits::default());
            reader.negotiate_frames();
            let mut fds = FdReceiver::default();
            let mut pool = SharedBufferPool::default();
            let mut mappings = SharedBufferMappings::default();
            // Kept regions (reused), and one past the keep budget.
            for (request, len) in [(1, 300_000), (2, 9_000_000), (3, 300_000), (4, 20_000_000)] {
                let body = vec![request as u8; len];
                let staged = pool.stage(0, call(request), &body).unwrap().unwrap();
                let passed: Vec<_> = staged.fd.iter().map(AsRawFd::as_raw_fd).collect();
                fds::send_all(sender.as_raw_fd(), &staged.frame, &passed).unwrap();
                let got = receive(&receiver, &mut reader, &mut fds, &mut mappings).unwrap();
                assert!(got == body);
                pool.release(request);
            }
            // A refused descriptor closes too.
            let file = std::fs::File::open("/dev/null").unwrap();
            let descriptor =
                SharedBufferDescriptor { call: call(5), body_len: 1, region: 3, attached: true };
            assert!(mappings.receive(1 << 20, &descriptor, Some(OwnedFd::from(file))).is_err());
            #[cfg(target_os = "linux")]
            assert!(region_mappings() > 0, "the kept regions are mapped on both ends");
        }
        assert_eq!(open_descriptors(), before, "a descriptor stayed open");
        assert_eq!(region_mappings(), 0, "a region stayed mapped");
    }
}
