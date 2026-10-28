//! SHARED_BUFFER regions over a real socket pair: one frame and one
//! descriptor carry a 10 MB body, the receiver's copy does not change when
//! the sender writes the region afterwards, reused regions pass no second
//! descriptor, and every descriptor or mapping that could fault the
//! receiver is refused.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use crate::fds::{self, FdReceiver};
use crate::shared_buffer::{
    DESCRIPTOR_LEN, INLINE_MAX, KEEP_BYTES, LAYOUT, MAX_REGIONS, SharedBufferDescriptor,
    SharedBufferMappings, SharedBufferPool, SharedBufferRefusal, SharedRegion, keeps,
    region_bytes_for,
};
use crate::{CallAux, CallFlags, Frame, FrameKind, FrameReader, Message, PriorityHint};

use super::framed_reader;

fn call(request_id: u32) -> CallAux {
    CallAux {
        request_id,
        method_id: 0x0101,
        priority: PriorityHint::Control,
        flags: CallFlags::empty(),
    }
}

fn body(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|at| (at as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

/// The two ends of one connection: what the sender writes, the frames and
/// descriptors the receiver cuts.
struct Wire {
    sender: UnixStream,
    receiver: UnixStream,
    reader: FrameReader,
    fds: FdReceiver,
}

impl Wire {
    fn new() -> Wire {
        let (sender, receiver) = UnixStream::pair().unwrap();
        Wire { sender, receiver, reader: framed_reader(), fds: FdReceiver::default() }
    }

    fn send(&self, bytes: &[u8], fd: Option<std::os::fd::BorrowedFd<'_>>) {
        let passed: Vec<_> = fd.iter().map(AsRawFd::as_raw_fd).collect();
        fds::send_all(self.sender.as_raw_fd(), bytes, &passed).unwrap();
    }

    /// The next frame, reading as needed.
    fn frame(&mut self) -> Frame {
        loop {
            if let Some(message) = self.reader.next_message().unwrap() {
                match message {
                    Message::Frame(frame) => return frame,
                    Message::Json(line) => panic!("unexpected line {line:?}"),
                }
            }
            let Wire { receiver, reader, fds, .. } = self;
            let read =
                reader.read_with(|room| fds.recv_into(receiver.as_raw_fd(), room, |_| {})).unwrap();
            assert_ne!(read, 0, "the sender closed");
        }
    }

    /// The next frame's body through `mappings`, taking the descriptor an
    /// attached frame carries.
    #[allow(
        clippy::panic_in_result_fn,
        reason = "a test helper: a frame of another kind fails the test"
    )]
    fn receive(
        &mut self,
        mappings: &mut SharedBufferMappings,
    ) -> Result<Vec<u8>, SharedBufferRefusal> {
        let frame = self.frame();
        assert_eq!(frame.header().kind(), FrameKind::SharedBuffer);
        let crate::Aux::SharedBufferSize(region_bytes) = frame.header().aux() else {
            panic!("aux of a shared buffer")
        };
        let descriptor = SharedBufferDescriptor::decode(frame.payload())?;
        let fd = if descriptor.attached { self.fds.take(1).unwrap().pop() } else { None };
        mappings.receive(region_bytes, &descriptor, fd)
    }
}

#[test]
fn shared_buffer_descriptor_bytes_are_the_documented_layout() {
    let descriptor = SharedBufferDescriptor {
        call: CallAux {
            request_id: 0x0403_0201,
            method_id: 0x0605,
            priority: PriorityHint::Bulk,
            flags: CallFlags::FLOW,
        },
        body_len: 0x0a09_0807,
        region: 3,
        attached: true,
    };
    let bytes = descriptor.encode();
    assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 2, 0x04, 7, 8, 9, 10, 3, 0, LAYOUT, 1]);
    assert_eq!(SharedBufferDescriptor::decode(&bytes), Ok(descriptor));
    let frame = descriptor.frame(0, 1 << 20).unwrap();
    assert_eq!(frame[..4], [0xFF, 10, 0, 0]);
    assert_eq!(frame[4..8], (DESCRIPTOR_LEN as u32).to_le_bytes());
    assert_eq!(frame[8..16], (1u64 << 20).to_le_bytes());
    assert_eq!(frame[16..], bytes);
}

#[test]
fn shared_buffer_descriptor_refuses_what_it_does_not_define() {
    let good = SharedBufferDescriptor { call: call(1), body_len: 9, region: 0, attached: false };
    let bytes = good.encode();
    assert_eq!(
        SharedBufferDescriptor::decode(&bytes[..15]),
        Err(SharedBufferRefusal::DescriptorLength { len: 15 })
    );
    let mut layout = bytes;
    layout[14] = LAYOUT + 1;
    assert_eq!(
        SharedBufferDescriptor::decode(&layout),
        Err(SharedBufferRefusal::Layout { layout: LAYOUT + 1 })
    );
    let mut flags = bytes;
    flags[15] = 0x02;
    assert_eq!(
        SharedBufferDescriptor::decode(&flags),
        Err(SharedBufferRefusal::UnknownFlags { flags: 0x02 })
    );
    let mut region = bytes;
    region[12] = MAX_REGIONS as u8;
    assert_eq!(
        SharedBufferDescriptor::decode(&region),
        Err(SharedBufferRefusal::RegionOutOfRange { region: MAX_REGIONS as u16 })
    );
    let mut priority = bytes;
    priority[6] = 9;
    assert_eq!(
        SharedBufferDescriptor::decode(&priority),
        Err(SharedBufferRefusal::Call(crate::BadMessage::UnknownPriority { hint: 9 }))
    );
}

#[test]
fn shared_buffer_region_sizes_and_the_keep_rule() {
    assert_eq!(region_bytes_for(INLINE_MAX + 1), Some(256 * 1024));
    assert_eq!(region_bytes_for(10_000_000), Some(16 * 1024 * 1024));
    assert_eq!(region_bytes_for(32 * 1024 * 1024), Some(32 * 1024 * 1024));
    assert_eq!(region_bytes_for(32 * 1024 * 1024 + 1), None);
    assert!(keeps(0, KEEP_BYTES));
    assert!(!keeps(1, KEEP_BYTES));
}

#[test]
fn shared_buffer_ten_mb_paste_is_one_frame_and_one_descriptor() {
    let mut wire = Wire::new();
    let mut pool = SharedBufferPool::default();
    let mut mappings = SharedBufferMappings::default();
    let paste = body(10 * 1024 * 1024, 7);
    let staged = pool.stage(0, call(1), &paste).unwrap().expect("a region");
    assert!(staged.fd.is_some(), "a new region attaches its descriptor");
    wire.send(&staged.frame, staged.fd);
    let got = wire.receive(&mut mappings).unwrap();
    assert_eq!(got.len(), paste.len());
    assert!(got == paste, "the body arrived unchanged");
    assert_eq!(wire.fds.queued(), 0, "one descriptor, taken by the frame");
    assert_eq!(wire.reader.next_message().unwrap(), None, "nothing but the one frame");
    assert!(mappings.is_mapped(0), "a 16 MiB region stays for the next paste");
}

#[test]
fn shared_buffer_a_write_after_the_copy_does_not_change_the_body() {
    let mut wire = Wire::new();
    let mut pool = SharedBufferPool::default();
    let mut mappings = SharedBufferMappings::default();
    let first = body(INLINE_MAX * 3, 1);
    let staged = pool.stage(0, call(1), &first).unwrap().unwrap();
    wire.send(&staged.frame, staged.fd);
    let got = wire.receive(&mut mappings).unwrap();
    pool.overwrite(0, &body(INLINE_MAX * 3, 99)).unwrap();
    assert!(got == first, "the receiver's bytes are its own copy");
}

#[test]
fn shared_buffer_a_reused_region_passes_no_second_descriptor() {
    let mut wire = Wire::new();
    let mut pool = SharedBufferPool::default();
    let mut mappings = SharedBufferMappings::default();
    let first = body(INLINE_MAX * 4, 1);
    let staged = pool.stage(0, call(1), &first).unwrap().unwrap();
    wire.send(&staged.frame, staged.fd);
    assert!(wire.receive(&mut mappings).unwrap() == first);
    // Busy until its reply: a second body takes another region.
    let second = body(INLINE_MAX * 2, 2);
    let staged = pool.stage(0, call(2), &second).unwrap().unwrap();
    assert_eq!((staged.region, staged.fd.is_some()), (1, true));
    wire.send(&staged.frame, staged.fd);
    assert!(wire.receive(&mut mappings).unwrap() == second);
    pool.release(1);
    let third = body(INLINE_MAX * 4 - 5, 3);
    let staged = pool.stage(0, call(3), &third).unwrap().unwrap();
    assert_eq!((staged.region, staged.fd.is_some()), (0, false));
    wire.send(&staged.frame, staged.fd);
    assert!(wire.receive(&mut mappings).unwrap() == third);
    assert_eq!(pool.regions(), 2);
}

#[test]
fn shared_buffer_small_bodies_and_a_busy_pool_stay_inline() {
    let mut pool = SharedBufferPool::default();
    assert!(pool.stage(0, call(1), &body(INLINE_MAX, 0)).unwrap().is_none());
    for request in 0..MAX_REGIONS as u32 {
        assert!(pool.stage(0, call(request), &body(INLINE_MAX + 1, 0)).unwrap().is_some());
    }
    assert!(pool.stage(0, call(9), &body(INLINE_MAX + 1, 0)).unwrap().is_none());
    pool.release(2);
    assert_eq!(
        pool.stage(0, call(9), &body(INLINE_MAX + 1, 0)).unwrap().map(|s| s.region),
        Some(2)
    );
}

#[test]
fn shared_buffer_a_region_past_the_keep_budget_maps_for_one_body() {
    let mut wire = Wire::new();
    let mut pool = SharedBufferPool::default();
    let mut mappings = SharedBufferMappings::default();
    // A 32 MiB region is above KEEP_BYTES: both ends drop it after its body.
    let big = body(20 * 1024 * 1024, 4);
    let staged = pool.stage(0, call(1), &big).unwrap().unwrap();
    wire.send(&staged.frame, staged.fd);
    assert!(wire.receive(&mut mappings).unwrap() == big);
    assert!(!mappings.is_mapped(0));
    assert_eq!(mappings.mapped_bytes(), 0);
    pool.release(1);
    assert_eq!(pool.regions(), 0);
    let staged = pool.stage(0, call(2), &big).unwrap().unwrap();
    assert!(staged.fd.is_some(), "the next body announces a new region");
}

#[test]
fn shared_buffer_regions_stay_within_the_keep_budget_on_both_ends() {
    let mut wire = Wire::new();
    let mut pool = SharedBufferPool::default();
    let mut mappings = SharedBufferMappings::default();
    // 8 MiB, 8 MiB: kept (16 MiB). A third 8 MiB region is mapped once.
    for request in 1..=3u32 {
        let next = body(5 * 1024 * 1024, request as u8);
        let staged = pool.stage(0, call(request), &next).unwrap().unwrap();
        wire.send(&staged.frame, staged.fd);
        assert!(wire.receive(&mut mappings).unwrap() == next);
    }
    assert_eq!(mappings.mapped_bytes(), KEEP_BYTES);
    for request in 1..=3 {
        pool.release(request);
    }
    assert_eq!(pool.regions(), 2, "the sender dropped the region the receiver did not keep");
    // Both kept regions serve the next bodies without a descriptor.
    for request in 4..=5u32 {
        let next = body(5 * 1024 * 1024, request as u8);
        let staged = pool.stage(0, call(request), &next).unwrap().unwrap();
        assert!(staged.fd.is_none());
        wire.send(&staged.frame, staged.fd);
        assert!(wire.receive(&mut mappings).unwrap() == next);
    }
}

/// A read-only descriptor of an object the sender could still shrink.
#[cfg(target_os = "linux")]
fn unsealed_read_only() -> OwnedFd {
    use std::os::fd::FromRawFd;
    // SAFETY: memfd_create returns a new descriptor or -1.
    let fd = unsafe { libc::memfd_create(c"unsealed".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: a fresh descriptor this test owns.
    let writable = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: sizes the memfd this test owns.
    assert_eq!(unsafe { libc::ftruncate(writable.as_raw_fd(), 1 << 20) }, 0);
    let path = std::ffi::CString::new(format!("/proc/self/fd/{fd}")).unwrap();
    // SAFETY: opens a NUL-terminated path.
    let read_only = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    assert!(read_only >= 0);
    // SAFETY: a fresh descriptor this test owns.
    unsafe { OwnedFd::from_raw_fd(read_only) }
}

fn attached(region: u16, body_len: u32) -> SharedBufferDescriptor {
    SharedBufferDescriptor { call: call(1), body_len, region, attached: true }
}

#[test]
fn shared_buffer_refuses_descriptors_that_could_fault_the_receiver() {
    let mut mappings = SharedBufferMappings::default();
    let region = SharedRegion::create(1 << 20).unwrap();
    let dup = || region.read_only_fd().try_clone_to_owned().unwrap();

    // A file and an unsealed memfd can shrink under a mapping.
    #[cfg(target_os = "linux")]
    assert_eq!(
        mappings.receive(1 << 20, &attached(0, 1), Some(unsealed_read_only())),
        Err(SharedBufferRefusal::NotSealed { region: 0 })
    );
    let file = std::fs::File::open("/dev/null").unwrap();
    assert_eq!(
        mappings.receive(1 << 20, &attached(0, 1), Some(OwnedFd::from(file))),
        Err(SharedBufferRefusal::NotSealed { region: 0 })
    );
    // A writable descriptor.
    let writable = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
    assert_eq!(
        mappings.receive(1 << 20, &attached(0, 1), Some(OwnedFd::from(writable))),
        Err(SharedBufferRefusal::Writable { region: 0 })
    );
    // A size larger than the object, or none.
    assert_eq!(
        mappings.receive(2 << 20, &attached(0, 1), Some(dup())),
        Err(SharedBufferRefusal::SizeMismatch { region: 0, declared: 2 << 20, actual: 1 << 20 })
    );
    assert_eq!(
        mappings.receive(0, &attached(0, 1), Some(dup())),
        Err(SharedBufferRefusal::RegionSize { region: 0, bytes: 0 })
    );
    assert_eq!(
        mappings.receive(1 << 20, &attached(1, 1), None),
        Err(SharedBufferRefusal::MissingFd { region: 1 })
    );
    // Reuse of a region nobody attached.
    let reuse = SharedBufferDescriptor { attached: false, ..attached(2, 1) };
    assert_eq!(
        mappings.receive(1 << 20, &reuse, None),
        Err(SharedBufferRefusal::UnknownRegion { region: 2 })
    );
    // A body past its region, then a reuse that names another size.
    assert_eq!(
        mappings.receive(1 << 20, &attached(0, (1 << 20) + 1), Some(dup())),
        Err(SharedBufferRefusal::BodyTooLong {
            region: 0,
            body_len: (1 << 20) + 1,
            region_bytes: 1 << 20
        })
    );
    let reuse = SharedBufferDescriptor { attached: false, ..attached(0, 1) };
    assert_eq!(
        mappings.receive(1 << 19, &reuse, None),
        Err(SharedBufferRefusal::SizeMismatch { region: 0, declared: 1 << 19, actual: 1 << 20 })
    );
    assert_eq!(mappings.receive(1 << 20, &reuse, None).map(|got| got.len()), Ok(1));
}

#[test]
fn shared_buffer_regions_cannot_be_resized_after_creation() {
    let region = SharedRegion::create(1 << 20).unwrap();
    // The read-only descriptor cannot truncate, and on Linux the seals refuse
    // it for every descriptor of the object.
    // SAFETY: ftruncate on a descriptor this test borrows.
    assert_ne!(unsafe { libc::ftruncate(region.read_only_fd().as_raw_fd(), 0) }, 0);
    #[cfg(target_os = "linux")]
    {
        // SAFETY: F_GET_SEALS reads the region's seals.
        let seals = unsafe { libc::fcntl(region.read_only_fd().as_raw_fd(), libc::F_GET_SEALS) };
        let wanted = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        assert_eq!(seals & wanted, wanted);
    }
}
