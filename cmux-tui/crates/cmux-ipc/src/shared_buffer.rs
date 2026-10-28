//! SHARED_BUFFER: a request body in a pooled shared memory region
//! (plans/cmux-next/chrome-parity/p05-ipc.md, section 3.5; task
//! p05-ipc-115).
//!
//! Chrome's `mojo_base::BigBuffer` sends a payload of up to
//! `kMaxInlineBytes` (64 KiB) inline and puts a larger one in shared memory
//! (chromium mojo/public/cpp/base/big_buffer.h). A receiver that does not
//! trust the sender copies the bytes out before it reads them, so the sender
//! cannot change what was validated (big_buffer.h: "make a copy of the data
//! before processing it"). cmux does the same for app requests above
//! [`INLINE_MAX`], with one change: BigBuffer maps a new region per message,
//! and the bench of this task (`examples/shared_buffer_bench.rs`) measured
//! that the per-message map and page faults eat most of the win. So each
//! connection keeps a pool of at most [`MAX_REGIONS`] regions on both ends,
//! as a Mojo data pipe keeps one region for its life
//! (mojo/core/data_pipe_producer_dispatcher.cc).
//!
//! The frame: kind SHARED_BUFFER on the request's stream, aux the region's
//! byte size, and a [`DESCRIPTOR_LEN`]-byte payload ([`SharedBufferDescriptor`]):
//!
//! ```text
//! bytes  0..8   the request's call fields (request id u32, method id u16,
//!               priority hint u8, flags u8), as a REQUEST frame's aux
//! bytes  8..12  body length u32 (<= the region size, <= MAX_FRAME_PAYLOAD)
//! bytes 12..14  region id u16 (< MAX_REGIONS)
//! byte  14      layout version (LAYOUT)
//! byte  15      flags: 0x01 ATTACHED, the frame carries the region's fd
//! ```
//!
//! An ATTACHED frame passes the region's read-only descriptor with its first
//! byte (`fds`, `SCM_RIGHTS`) and replaces any region the id named before.
//! A frame without it reuses the region its id names. The receiver copies
//! the body out of its read-only mapping before anything parses it, so a
//! 10 MB paste is one frame, one descriptor and one copy on each side.
//!
//! Which regions stay mapped is decided by both ends with the same rule
//! ([`keeps`]): after its body is copied, a region stays when it and the
//! other kept regions hold at most [`KEEP_BYTES`]; otherwise it is mapped
//! for this one body and the sender announces a new region next time. Both
//! ends see the same frames in the same order, so they always agree.
//!
//! A region the receiver maps must not shrink under its mapping (a read past
//! the end of a shrunk object raises SIGBUS). The receiver maps only a
//! read-only descriptor of an object whose size is fixed: on Linux a memfd
//! sealed with `F_SEAL_SHRINK`; on macOS a POSIX shm object, which xnu sizes
//! once (`pshm_truncate` refuses a second `ftruncate`), told apart by
//! `fstat` (xnu's `pshm_stat` reports no file type; every vnode, the only
//! kind that can shrink under a mapping, reports one). Chrome
//! checks a received region's mode the same way
//! (base/memory/platform_shared_memory_region_posix.cc,
//! `CheckPlatformHandlePermissionsCorrespondToMode`). Shared buffers ride
//! only the `shared-buffers-v1` grant, which needs `ipc-frames-v2`: a direct
//! same-user local peer (spec/transports.md, "Framed IPC grants").

use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use crate::frame::{
    Aux, BadMessage, CallAux, FrameHeader, FrameKind, HEADER_LEN, MAX_FRAME_PAYLOAD,
};

/// The largest body a request sends inline: Mojo's `BigBuffer`
/// `kMaxInlineBytes`. A larger one goes in a region when the pool has one.
pub const INLINE_MAX: usize = 64 * 1024;

/// Regions one connection may name (region ids `0..MAX_REGIONS`).
pub const MAX_REGIONS: usize = 4;

/// The smallest region the pool makes; a region is a power of two at least
/// this large, so a few sizes serve every body.
pub const MIN_REGION_BYTES: u64 = 256 * 1024;

/// The largest region: one frame's payload bound.
pub const MAX_REGION_BYTES: u64 = MAX_FRAME_PAYLOAD as u64;

/// What the kept regions of one connection may hold together ([`keeps`]).
pub const KEEP_BYTES: u64 = 16 * 1024 * 1024;

/// The SHARED_BUFFER payload: [`SharedBufferDescriptor`].
pub const DESCRIPTOR_LEN: usize = 16;

/// A whole SHARED_BUFFER frame: header and descriptor.
pub const FRAME_LEN: usize = HEADER_LEN + DESCRIPTOR_LEN;

/// The descriptor layout this build writes and reads.
pub const LAYOUT: u8 = 1;

/// The size of the region a body of `body_len` bytes goes in, or `None`
/// when it is larger than [`MAX_REGION_BYTES`].
pub fn region_bytes_for(body_len: usize) -> Option<u64> {
    let len = u64::try_from(body_len).ok()?;
    (len <= MAX_REGION_BYTES)
        .then(|| len.next_power_of_two().clamp(MIN_REGION_BYTES, MAX_REGION_BYTES))
}

/// Whether a region of `bytes` stays mapped after its body was copied, when
/// the other kept regions hold `others_kept` bytes. Both ends apply it.
pub fn keeps(others_kept: u64, bytes: u64) -> bool {
    others_kept.saturating_add(bytes) <= KEEP_BYTES
}

/// The payload of a SHARED_BUFFER frame (the layout is in the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedBufferDescriptor {
    /// The request's call fields, as its REQUEST frame would carry them.
    pub call: CallAux,
    /// Bytes of the body at the start of the region.
    pub body_len: u32,
    /// Which of the connection's regions holds the body.
    pub region: u16,
    /// The frame carries the region's descriptor, which replaces the region
    /// the id named before.
    pub attached: bool,
}

impl SharedBufferDescriptor {
    const ATTACHED: u8 = 0x01;

    /// The 16 payload bytes.
    pub fn encode(&self) -> [u8; DESCRIPTOR_LEN] {
        let [c0, c1, c2, c3, c4, c5, c6, c7] = self.call.to_wire();
        let [b0, b1, b2, b3] = self.body_len.to_le_bytes();
        let [r0, r1] = self.region.to_le_bytes();
        let flags = if self.attached { Self::ATTACHED } else { 0 };
        [c0, c1, c2, c3, c4, c5, c6, c7, b0, b1, b2, b3, r0, r1, LAYOUT, flags]
    }

    /// Decodes a SHARED_BUFFER payload, refusing another length, layout,
    /// flag bit, call field or a region id past [`MAX_REGIONS`].
    pub fn decode(payload: &[u8]) -> Result<SharedBufferDescriptor, SharedBufferRefusal> {
        let bytes: [u8; DESCRIPTOR_LEN] = payload
            .try_into()
            .map_err(|_| SharedBufferRefusal::DescriptorLength { len: payload.len() })?;
        let [c0, c1, c2, c3, c4, c5, c6, c7, b0, b1, b2, b3, r0, r1, layout, flags] = bytes;
        if layout != LAYOUT {
            return Err(SharedBufferRefusal::Layout { layout });
        }
        if flags & !Self::ATTACHED != 0 {
            return Err(SharedBufferRefusal::UnknownFlags { flags });
        }
        let call = CallAux::from_wire([c0, c1, c2, c3, c4, c5, c6, c7])
            .map_err(SharedBufferRefusal::Call)?;
        let region = u16::from_le_bytes([r0, r1]);
        if usize::from(region) >= MAX_REGIONS {
            return Err(SharedBufferRefusal::RegionOutOfRange { region });
        }
        Ok(SharedBufferDescriptor {
            call,
            body_len: u32::from_le_bytes([b0, b1, b2, b3]),
            region,
            attached: flags & Self::ATTACHED != 0,
        })
    }

    /// The whole frame on `stream` for a region of `region_bytes`.
    pub fn frame(&self, stream: u16, region_bytes: u64) -> Result<[u8; FRAME_LEN], BadMessage> {
        let header = FrameHeader::new(
            FrameKind::SharedBuffer,
            stream,
            DESCRIPTOR_LEN as u32,
            Aux::SharedBufferSize(region_bytes),
        )?;
        let mut frame = [0u8; FRAME_LEN];
        let (head, tail) = frame.split_at_mut(HEADER_LEN);
        head.copy_from_slice(&header.encode());
        tail.copy_from_slice(&self.encode());
        Ok(frame)
    }
}

/// Why a SHARED_BUFFER frame closed its connection. Like every bad message,
/// nothing after it is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedBufferRefusal {
    /// A payload that is not [`DESCRIPTOR_LEN`] bytes.
    DescriptorLength { len: usize },
    /// A descriptor layout this build does not read.
    Layout { layout: u8 },
    /// A flag bit outside ATTACHED.
    UnknownFlags { flags: u8 },
    /// The request's call fields do not decode.
    Call(BadMessage),
    /// A region id at or past [`MAX_REGIONS`].
    RegionOutOfRange { region: u16 },
    /// An ATTACHED frame with no descriptor queued.
    MissingFd { region: u16 },
    /// A frame that reuses a region no frame attached (or a region that was
    /// mapped for one body only).
    UnknownRegion { region: u16 },
    /// A region size of 0 or above [`MAX_REGION_BYTES`].
    RegionSize { region: u16, bytes: u64 },
    /// The frame's region size differs from the region's: smaller than the
    /// attached object, or not the size the region was attached with.
    SizeMismatch { region: u16, declared: u64, actual: u64 },
    /// A body longer than its region.
    BodyTooLong { region: u16, body_len: u32, region_bytes: u64 },
    /// A descriptor that may be written: only read-only ones are mapped.
    Writable { region: u16 },
    /// A descriptor whose object could shrink under the mapping: not a
    /// sealed memfd (Linux) or a POSIX shm object (macOS).
    NotSealed { region: u16 },
    /// `fstat` or `mmap` failed with this errno.
    Os { region: u16, errno: i32 },
}

impl SharedBufferRefusal {
    /// The stable `bad_message` reason.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::DescriptorLength { .. } => "shared_buffer_descriptor_length",
            Self::Layout { .. } => "shared_buffer_layout",
            Self::UnknownFlags { .. } => "shared_buffer_flags",
            Self::Call(bad) => bad.reason(),
            Self::RegionOutOfRange { .. } => "shared_buffer_region_out_of_range",
            Self::MissingFd { .. } => "shared_buffer_missing_fd",
            Self::UnknownRegion { .. } => "shared_buffer_unknown_region",
            Self::RegionSize { .. } => "shared_buffer_region_size",
            Self::SizeMismatch { .. } => "shared_buffer_size_mismatch",
            Self::BodyTooLong { .. } => "shared_buffer_body_too_long",
            Self::Writable { .. } => "shared_buffer_writable",
            Self::NotSealed { .. } => "shared_buffer_not_sealed",
            Self::Os { .. } => "shared_buffer_os",
        }
    }
}

impl fmt::Display for SharedBufferRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DescriptorLength { len } => {
                write!(f, "shared buffer descriptor of {len} bytes, expected {DESCRIPTOR_LEN}")
            }
            Self::Layout { layout } => {
                write!(f, "shared buffer layout {layout}, expected {LAYOUT}")
            }
            Self::UnknownFlags { flags } => write!(f, "shared buffer flags {flags:#010b}"),
            Self::Call(bad) => write!(f, "shared buffer call fields: {bad}"),
            Self::RegionOutOfRange { region } => {
                write!(f, "shared buffer region {region} is not below {MAX_REGIONS}")
            }
            Self::MissingFd { region } => {
                write!(f, "shared buffer region {region} attached no descriptor")
            }
            Self::UnknownRegion { region } => {
                write!(f, "shared buffer region {region} is not mapped")
            }
            Self::RegionSize { region, bytes } => write!(
                f,
                "shared buffer region {region} of {bytes} bytes is not in 1..={MAX_REGION_BYTES}"
            ),
            Self::SizeMismatch { region, declared, actual } => write!(
                f,
                "shared buffer region {region} declared {declared} bytes, its object has {actual}"
            ),
            Self::BodyTooLong { region, body_len, region_bytes } => write!(
                f,
                "shared buffer body of {body_len} bytes exceeds region {region} of {region_bytes}"
            ),
            Self::Writable { region } => {
                write!(f, "shared buffer region {region} came with a writable descriptor")
            }
            Self::NotSealed { region } => {
                write!(f, "shared buffer region {region} is not a fixed-size shared memory object")
            }
            Self::Os { region, errno } => {
                write!(f, "shared buffer region {region}: OS error {errno}")
            }
        }
    }
}

impl std::error::Error for SharedBufferRefusal {}

/// The receiving end's regions: one read-only mapping per region id. Owned
/// by the connection's reader; dropping it unmaps every region.
#[derive(Debug, Default)]
pub struct SharedBufferMappings {
    regions: [Option<Mapping>; MAX_REGIONS],
}

impl SharedBufferMappings {
    /// The body of one SHARED_BUFFER frame, copied out of its region before
    /// anyone reads it. `region_bytes` is the frame's aux; `fd` is the
    /// descriptor the frame carried, when `descriptor.attached`.
    pub fn receive(
        &mut self,
        region_bytes: u64,
        descriptor: &SharedBufferDescriptor,
        fd: Option<OwnedFd>,
    ) -> Result<Vec<u8>, SharedBufferRefusal> {
        let region = descriptor.region;
        let index = usize::from(region);
        if index >= MAX_REGIONS {
            return Err(SharedBufferRefusal::RegionOutOfRange { region });
        }
        if descriptor.attached {
            let fd = fd.ok_or(SharedBufferRefusal::MissingFd { region })?;
            let mut mapping = Mapping::adopt(fd.as_fd(), region, region_bytes)?;
            // The mapping keeps the object; the descriptor closes here.
            drop(fd);
            mapping.kept = keeps(self.kept_bytes_except(index), mapping.len);
            if let Some(slot) = self.regions.get_mut(index) {
                *slot = Some(mapping);
            }
        }
        let slot =
            self.regions.get_mut(index).ok_or(SharedBufferRefusal::RegionOutOfRange { region })?;
        let mapping = slot.as_ref().ok_or(SharedBufferRefusal::UnknownRegion { region })?;
        if mapping.len != region_bytes {
            return Err(SharedBufferRefusal::SizeMismatch {
                region,
                declared: region_bytes,
                actual: mapping.len,
            });
        }
        let body_len = descriptor.body_len;
        if u64::from(body_len) > mapping.len {
            return Err(SharedBufferRefusal::BodyTooLong {
                region,
                body_len,
                region_bytes: mapping.len,
            });
        }
        let body = mapping.copy_prefix(body_len as usize);
        if !mapping.kept {
            *slot = None;
        }
        Ok(body)
    }

    /// Bytes of every region mapped now: between frames, the kept ones.
    pub fn mapped_bytes(&self) -> u64 {
        self.regions.iter().flatten().map(|mapping| mapping.len).sum()
    }

    /// Whether region `region` is mapped.
    pub fn is_mapped(&self, region: u16) -> bool {
        self.regions.get(usize::from(region)).is_some_and(Option::is_some)
    }

    fn kept_bytes_except(&self, index: usize) -> u64 {
        self.regions
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != index)
            .filter_map(|(_, mapping)| mapping.as_ref())
            .filter(|mapping| mapping.kept)
            .map(|mapping| mapping.len)
            .sum()
    }
}

/// One read-only `MAP_SHARED` mapping of a whole region.
#[derive(Debug)]
struct Mapping {
    base: NonNull<u8>,
    len: u64,
    /// Stays mapped after the body that attached it ([`keeps`]).
    kept: bool,
}

// SAFETY: the mapping is address space this value owns alone; nothing ties
// it to the thread that mapped it, and it is only read through `&self`.
unsafe impl Send for Mapping {}

impl Mapping {
    /// Maps `fd` read-only after checking that it is a read-only descriptor
    /// of a fixed-size object of at least `bytes`.
    fn adopt(fd: BorrowedFd<'_>, region: u16, bytes: u64) -> Result<Mapping, SharedBufferRefusal> {
        if bytes == 0 || bytes > MAX_REGION_BYTES {
            return Err(SharedBufferRefusal::RegionSize { region, bytes });
        }
        let raw = fd.as_raw_fd();
        // SAFETY: F_GETFL reads the descriptor's status flags.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
        if flags < 0 {
            return Err(os_refusal(region));
        }
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(SharedBufferRefusal::Writable { region });
        }
        if !fixed_size(raw) {
            return Err(SharedBufferRefusal::NotSealed { region });
        }
        // SAFETY: a zeroed stat is a valid buffer for fstat to fill.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat writes one stat into `stat`.
        if unsafe { libc::fstat(raw, &mut stat) } != 0 {
            return Err(os_refusal(region));
        }
        let actual = u64::try_from(stat.st_size).unwrap_or(0);
        if actual < bytes {
            return Err(SharedBufferRefusal::SizeMismatch { region, declared: bytes, actual });
        }
        let len = usize::try_from(bytes)
            .map_err(|_| SharedBufferRefusal::RegionSize { region, bytes })?;
        // SAFETY: maps `len` bytes of an object of at least `len` bytes whose
        // size cannot change (checked above), read-only and shared.
        let base = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, raw, 0)
        };
        if base == libc::MAP_FAILED {
            return Err(os_refusal(region));
        }
        let base =
            NonNull::new(base.cast::<u8>()).ok_or(SharedBufferRefusal::Os { region, errno: 0 })?;
        Ok(Mapping { base, len: bytes, kept: false })
    }

    /// The first `count` bytes, copied (`count <= len`, checked by the caller).
    fn copy_prefix(&self, count: usize) -> Vec<u8> {
        let mut body = Vec::with_capacity(count);
        // SAFETY: `count` is at most the mapping's length and the mapping is
        // readable for its whole length; `body` has room for `count` bytes.
        // The sender may write the region meanwhile: the copy then holds
        // whatever it wrote, and nothing reads the region again.
        unsafe {
            std::ptr::copy_nonoverlapping(self.base.as_ptr(), body.as_mut_ptr(), count);
            body.set_len(count);
        }
        body
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps the mapping this value made, once.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len as usize) };
    }
}

fn os_refusal(region: u16) -> SharedBufferRefusal {
    SharedBufferRefusal::Os {
        region,
        errno: io::Error::last_os_error().raw_os_error().unwrap_or(0),
    }
}

/// Whether `fd` is an object nobody can shrink: a memfd sealed against
/// shrinking.
#[cfg(target_os = "linux")]
fn fixed_size(fd: libc::c_int) -> bool {
    // SAFETY: F_GET_SEALS reads a memfd's seals; it fails on anything else.
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    seals >= 0 && seals & libc::F_SEAL_SHRINK != 0
}

/// Whether `fd` is an object nobody can shrink: a POSIX shm object, which
/// xnu sizes once. xnu's `pshm_stat` fills `st_mode` with the object's
/// permission bits only, so its file type is 0; a file, a pipe, a socket or
/// a device reports its type. A shm object created with type bits in its
/// mode is refused, which is safe.
#[cfg(target_vendor = "apple")]
fn fixed_size(fd: libc::c_int) -> bool {
    // SAFETY: a zeroed stat is a valid buffer for fstat to fill.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat writes one stat into `stat`.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return false;
    }
    stat.st_mode & libc::S_IFMT == 0
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn fixed_size(_fd: libc::c_int) -> bool {
    false
}

/// A region the sending end writes: a read-write mapping and the read-only
/// descriptor it passes. Dropping it unmaps and closes it.
#[derive(Debug)]
pub struct SharedRegion {
    base: NonNull<u8>,
    len: u64,
    read_only: OwnedFd,
}

// SAFETY: the mapping is address space this value owns alone; it is
// written only through `&mut self`.
unsafe impl Send for SharedRegion {}

impl SharedRegion {
    /// A zeroed region of `len` bytes (`1..=MAX_REGION_BYTES`): a sealed
    /// memfd on Linux, an unlinked POSIX shm object on macOS.
    pub fn create(len: u64) -> io::Result<SharedRegion> {
        if len == 0 || len > MAX_REGION_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "shared region size"));
        }
        let bytes =
            usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let (writable, read_only) = new_object(len)?;
        // SAFETY: maps `bytes` of an object just sized to `len`, read-write.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                writable.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // The mapping keeps the object; only the read-only descriptor stays.
        drop(writable);
        let base = NonNull::new(base.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(SharedRegion { base, len, read_only })
    }

    /// The region's size.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Always false: a region has at least one byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The read-only descriptor a frame attaches.
    pub fn read_only_fd(&self) -> BorrowedFd<'_> {
        self.read_only.as_fd()
    }

    /// Writes `bytes` at the start of the region; more than it holds is an
    /// `InvalidInput` error.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if u64::try_from(bytes.len()).map_or(true, |count| count > self.len) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "body larger than its region"));
        }
        // SAFETY: the mapping is writable for `len >= bytes.len()` bytes and
        // does not overlap `bytes`, which this process owns elsewhere.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.as_ptr(), bytes.len()) };
        Ok(())
    }
}

impl Drop for SharedRegion {
    fn drop(&mut self) {
        // SAFETY: unmaps the mapping this value made, once.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len as usize) };
    }
}

/// A memfd of `len` bytes sealed against resizing, and a read-only
/// descriptor of it (reopened through /proc, as Chrome's
/// `PlatformSharedMemoryRegion` makes a read-only memfd handle).
#[cfg(target_os = "linux")]
fn new_object(len: u64) -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: memfd_create returns a new descriptor or -1.
    let fd = unsafe {
        libc::memfd_create(
            c"cmux-shared-buffer".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor this function owns.
    let writable = unsafe { OwnedFd::from_raw_fd(fd) };
    let size =
        libc::off_t::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: sizes the memfd this function owns.
    if unsafe { libc::ftruncate(writable.as_raw_fd(), size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    // SAFETY: seals the memfd this function owns.
    if unsafe { libc::fcntl(writable.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let path = std::ffi::CString::new(format!("/proc/self/fd/{}", writable.as_raw_fd()))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: opens a NUL-terminated path; returns a new descriptor or -1.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor this function owns.
    Ok((writable, unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// An unlinked POSIX shm object of `len` bytes, opened read-write and
/// read-only before its name is removed (CmuxNextSharedRegionC's
/// `cmux_shared_buffer_region_create` is the app's copy in C).
#[cfg(target_vendor = "apple")]
fn new_object(len: u64) -> io::Result<(OwnedFd, OwnedFd)> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let size =
        libc::off_t::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    for _ in 0..16 {
        // At most 31 bytes (PSHMNAMLEN).
        let name =
            format!("/cmsb{:x}.{:x}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let name = std::ffi::CString::new(name)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL;
        // SAFETY: shm_open with a NUL-terminated name; the mode is variadic.
        let fd = unsafe { libc::shm_open(name.as_ptr(), flags, 0o600 as libc::c_uint) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(error);
        }
        // SAFETY: a fresh descriptor this function owns.
        let writable = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: as above, read-only.
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0 as libc::c_uint) };
        let opened = io::Error::last_os_error();
        // SAFETY: removes the name just made; the descriptors keep the object.
        unsafe { libc::shm_unlink(name.as_ptr()) };
        if fd < 0 {
            return Err(opened);
        }
        // SAFETY: a fresh descriptor this function owns.
        let read_only = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sizes the object once, which xnu then keeps.
        if unsafe { libc::ftruncate(writable.as_raw_fd(), size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // shm_open does not take O_CLOEXEC on every macOS (cmux-holder
        // ring.rs), so both are made close-on-exec here.
        for fd in [&writable, &read_only] {
            // SAFETY: F_SETFD on a descriptor this function owns.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        return Ok((writable, read_only));
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free shared buffer name"))
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn new_object(_len: u64) -> io::Result<(OwnedFd, OwnedFd)> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// The sending end's regions. [`stage`](Self::stage) writes a body into a
/// free region and returns its frame; [`release`](Self::release) frees the
/// region once the request's reply arrived (the receiver copied the body
/// before it ran the request, so the region is no longer read).
#[derive(Debug, Default)]
pub struct SharedBufferPool {
    slots: [Option<Slot>; MAX_REGIONS],
}

#[derive(Debug)]
struct Slot {
    region: SharedRegion,
    /// A frame attached this region's descriptor.
    announced: bool,
    /// The receiver keeps it mapped after the announcing body ([`keeps`]).
    kept: bool,
    /// The request whose body it holds until its reply.
    busy: Option<u32>,
}

/// One staged SHARED_BUFFER: the frame to write and, when it attaches its
/// region, the descriptor to pass with its first byte.
#[derive(Debug)]
pub struct Staged<'a> {
    pub frame: [u8; FRAME_LEN],
    pub fd: Option<BorrowedFd<'a>>,
    pub region: u16,
}

impl SharedBufferPool {
    /// Writes `body` (more than [`INLINE_MAX`] bytes) into a free region
    /// and returns its frame on `stream` for request `call`. `Ok(None)`
    /// sends it inline instead: a smaller or larger body, or every region
    /// busy. An error is the OS refusing a new region.
    pub fn stage(
        &mut self,
        stream: u16,
        call: CallAux,
        body: &[u8],
    ) -> io::Result<Option<Staged<'_>>> {
        if body.len() <= INLINE_MAX {
            return Ok(None);
        }
        let Some(need) = region_bytes_for(body.len()) else { return Ok(None) };
        let Some(index) = self.choose(body.len()) else { return Ok(None) };
        let fits = self.slots.get(index).and_then(Option::as_ref).is_some_and(|slot| {
            u64::try_from(body.len()).is_ok_and(|len| len <= slot.region.len())
        });
        if !fits {
            let region = SharedRegion::create(need)?;
            if let Some(slot) = self.slots.get_mut(index) {
                *slot = Some(Slot { region, announced: false, kept: false, busy: None });
            }
        }
        let others = self.kept_bytes_except(index);
        let Some(slot) = self.slots.get_mut(index).and_then(Option::as_mut) else {
            return Ok(None);
        };
        slot.region.write(body)?;
        let attached = !slot.announced;
        if attached {
            slot.announced = true;
            slot.kept = keeps(others, slot.region.len());
        }
        slot.busy = Some(call.request_id);
        let region =
            u16::try_from(index).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let body_len =
            u32::try_from(body.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let descriptor = SharedBufferDescriptor { call, body_len, region, attached };
        let frame = descriptor
            .frame(stream, slot.region.len())
            .map_err(|bad| io::Error::new(io::ErrorKind::InvalidInput, bad))?;
        let fd = attached.then(|| slot.region.read_only_fd());
        Ok(Some(Staged { frame, fd, region }))
    }

    /// Frees the region of request `request_id` after its reply; a region
    /// the receiver mapped for that body only is dropped.
    pub fn release(&mut self, request_id: u32) {
        for slot in &mut self.slots {
            if slot.as_ref().is_some_and(|held| held.busy == Some(request_id)) {
                let kept = slot.as_ref().is_some_and(|held| held.kept);
                if kept {
                    if let Some(held) = slot.as_mut() {
                        held.busy = None;
                    }
                } else {
                    *slot = None;
                }
            }
        }
    }

    /// Writes `bytes` into region `region` whatever it holds: a test of
    /// what a receiver sees when a sender changes a region after sending.
    pub fn overwrite(&mut self, region: u16, bytes: &[u8]) -> io::Result<()> {
        let slot = self
            .slots
            .get_mut(usize::from(region))
            .and_then(Option::as_mut)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        slot.region.write(bytes)
    }

    /// Regions the pool holds now.
    pub fn regions(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// The free region that fits `len` best; else an empty id; else the
    /// smallest free region, to be replaced by a larger one.
    fn choose(&self, len: usize) -> Option<usize> {
        let len = u64::try_from(len).ok()?;
        let free = || {
            self.slots
                .iter()
                .enumerate()
                .filter_map(|(index, slot)| slot.as_ref().map(|slot| (index, slot)))
                .filter(|(_, slot)| slot.busy.is_none())
        };
        free()
            .filter(|(_, slot)| slot.region.len() >= len)
            .min_by_key(|(_, slot)| slot.region.len())
            .map(|(index, _)| index)
            .or_else(|| self.slots.iter().position(Option::is_none))
            .or_else(|| free().min_by_key(|(_, slot)| slot.region.len()).map(|(index, _)| index))
    }

    fn kept_bytes_except(&self, index: usize) -> u64 {
        self.slots
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != index)
            .filter_map(|(_, slot)| slot.as_ref())
            .filter(|slot| slot.announced && slot.kept)
            .map(|slot| slot.region.len())
            .sum()
    }
}
