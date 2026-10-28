//! Shared memory segments that only descriptors reach: the holder's
//! per-terminal ring (`cmux-holder/src/ring.rs`) and the daemon's output ring
//! for the app ([`crate::output_ring`]).
//!
//! A segment is a `memfd` on Linux and an unlinked POSIX `shm_open` object
//! elsewhere. The `shm_open` name lives only inside the call that makes it
//! (`O_CREAT|O_EXCL`, mode 0600, unlinked before the call returns) and is at
//! most [`MAX_NAME_LEN`] bytes, the limit macOS puts on it (`PSHMNAMLEN`).
//! Every descriptor is close-on-exec from the moment it is opened where the
//! system takes the flag, and set so right after where it refuses it.
//!
//! A read-only segment ([`create_read_only`]) gives its writer the only
//! writable view: the writer gets a mapping, the readers a descriptor that
//! cannot map the segment writable. This follows Chromium's
//! `base/memory/platform_shared_memory_region_posix.cc`, which opens the
//! read-only descriptor `O_RDONLY` before it unlinks the file. On macOS the
//! read-only descriptor is the same `shm_open` name opened `O_RDONLY` before
//! the unlink. On Linux the protection is the `F_SEAL_FUTURE_WRITE` seal,
//! added after the writer mapped the memfd: no new writable mapping or
//! `write` through any descriptor, a reopened one too. A kernel without the
//! seal (before Linux 5.1) gets [`io::ErrorKind::Unsupported`], never a
//! writable reader: a memfd inode is mode 0777, so a descriptor reopened
//! `O_RDONLY` alone does not stop a reader from reopening it `O_RDWR`. The
//! readers get the memfd reopened through `/proc/self/fd` `O_RDONLY` as
//! defence in depth, or a duplicate of the sealed descriptor where there is
//! no `/proc`.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;

/// The longest `shm_open` name, leading slash included: macOS refuses a
/// longer one (`PSHMNAMLEN`, `ENAMETOOLONG`).
pub const MAX_NAME_LEN: usize = 31;

/// Which segment: it names the `memfd` (seen in `/proc/<pid>/fd` and
/// `/proc/<pid>/maps`) and prefixes the short-lived `shm_open` name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// The holder's ring of one terminal (layout `CMHR`).
    HolderRing,
    /// The daemon's output ring of one terminal for the app (layout `CMAR`).
    OutputRing,
}

impl SegmentKind {
    /// Five bytes, so every name [`segment_name`] makes fits [`MAX_NAME_LEN`].
    fn prefix(self) -> &'static str {
        match self {
            Self::HolderRing => "/cmhr",
            Self::OutputRing => "/cmar",
        }
    }

    #[cfg(target_os = "linux")]
    fn memfd_name(self) -> &'static std::ffi::CStr {
        match self {
            Self::HolderRing => c"cmux-holder-ring",
            Self::OutputRing => c"cmux-output-ring",
        }
    }
}

/// The `shm_open` name of a segment `pid` makes: its serial in the process
/// and the clock's nanoseconds keep two tries apart. Every value is at most
/// 8 hex digits, so the name is at most 5 + 8 + 1 + 8 + 1 + 8 = 31 bytes.
pub fn segment_name(kind: SegmentKind, pid: u32, serial: u32, nanos: u32) -> String {
    format!("{}{pid:x}.{serial:x}.{nanos:x}", kind.prefix())
}

/// How a mapping may be touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    ReadOnly,
    ReadWrite,
}

/// A shared mapping of a segment, unmapped on drop. It is reached only
/// through its raw pointer: another process may write the pages at any time.
#[derive(Debug)]
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain shared memory owned by this value until drop;
// it holds no thread-bound state, and every access goes through the raw
// pointer, whose users order it with atomics.
unsafe impl Send for Mapping {}
// SAFETY: as for Send: `&Mapping` hands out only the address and the length.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map `len` bytes of `fd` from its start, shared.
    pub fn new(fd: BorrowedFd<'_>, len: usize, protection: Protection) -> io::Result<Self> {
        let prot = match protection {
            Protection::ReadOnly => libc::PROT_READ,
            Protection::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
        };
        // SAFETY: maps `len` bytes of a live descriptor at an address the
        // kernel picks; nothing else owns the result.
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        NonNull::new(map.cast::<u8>())
            .map(|ptr| Self { ptr, len })
            .ok_or_else(|| io::Error::other("mmap returned a null mapping"))
    }

    /// The first byte of the mapping (page aligned).
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// The bytes mapped.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is empty (never, for a mapping `mmap` made).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps the mapping this value made, once.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// A new segment of `len` zero bytes, read-write, close-on-exec, with no
/// name anywhere: the holder's ring, and anything else one process writes
/// and hands on whole.
pub fn create(kind: SegmentKind, len: usize) -> io::Result<OwnedFd> {
    let segment = open_unsized(kind)?;
    // Already close-on-exec where the platform opened it so; else this
    // closes the rest of the window (see `open_named`).
    set_cloexec(segment.as_raw_fd())?;
    truncate(&segment, len)?;
    Ok(segment)
}

/// A segment its maker writes and other processes only read: the maker's
/// writable mapping and the descriptor to pass to readers.
#[derive(Debug)]
pub struct ReadOnlySegment {
    /// The only writable view: a read-write mapping of the whole segment.
    pub mapping: Mapping,
    /// A descriptor no process can map writable or `write` through.
    pub reader: OwnedFd,
}

/// A new segment of `len` zero bytes whose readers cannot write it (module
/// docs). The read-write descriptor is closed before this returns: only the
/// returned mapping writes the segment.
pub fn create_read_only(kind: SegmentKind, len: usize) -> io::Result<ReadOnlySegment> {
    read_only_segment(kind, len)
}

/// Whether `fd` can be mapped writable and shared: a reader refuses such a
/// descriptor (it could change what other readers see). The probe maps the
/// first page (one byte, so no object is too small for it) and unmaps it; it
/// never writes.
pub fn maps_writable(fd: BorrowedFd<'_>) -> bool {
    Mapping::new(fd, 1, Protection::ReadWrite).is_ok()
}

/// Mark a descriptor close-on-exec.
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor the caller owns.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn truncate(segment: &OwnedFd, len: usize) -> io::Result<()> {
    let size = libc::off_t::try_from(len).map_err(|_| invalid("segment too large"))?;
    // SAFETY: sizes a descriptor the caller owns.
    if unsafe { libc::ftruncate(segment.as_raw_fd(), size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Take ownership of a descriptor a libc call returned, or its error.
fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn memfd(kind: SegmentKind, flags: libc::c_uint) -> io::Result<OwnedFd> {
    // SAFETY: memfd_create with a NUL-terminated name returns a new
    // descriptor or -1.
    owned(unsafe { libc::memfd_create(kind.memfd_name().as_ptr(), libc::MFD_CLOEXEC | flags) })
}

#[cfg(target_os = "linux")]
fn open_unsized(kind: SegmentKind) -> io::Result<OwnedFd> {
    memfd(kind, 0)
}

#[cfg(not(target_os = "linux"))]
fn open_unsized(kind: SegmentKind) -> io::Result<OwnedFd> {
    open_named(kind, |_, _| Ok(())).map(|(segment, (), _)| segment)
}

/// Linux: map the memfd, then seal it against every later writable mapping,
/// `write`, resize and seal change, then reopen it read-only for readers.
#[cfg(target_os = "linux")]
fn read_only_segment(kind: SegmentKind, len: usize) -> io::Result<ReadOnlySegment> {
    let segment = memfd(kind, libc::MFD_ALLOW_SEALING)?;
    truncate(&segment, len)?;
    let mapping = Mapping::new(fd_of(&segment), len, Protection::ReadWrite)?;
    let fd = segment.as_raw_fd();
    let seal = |seals: libc::c_int| {
        // SAFETY: F_ADD_SEALS on a memfd this function owns.
        unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) == 0 }
    };
    // F_SEAL_FUTURE_WRITE needs Linux 5.1. Without it a reader could reopen
    // the memfd O_RDWR, so an older kernel gets no read-only segment.
    if !seal(libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_FUTURE_WRITE) {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINVAL) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "F_SEAL_FUTURE_WRITE needs Linux 5.1 or later",
            ));
        }
        return Err(error);
    }
    let reader = match reopen_read_only(fd) {
        Ok(reader) => reader,
        // No /proc (a sandbox): the sealed descriptor is read-only too.
        // SAFETY: F_DUPFD_CLOEXEC of a live descriptor returns a new one.
        Err(_) => owned(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })?,
    };
    if !seal(libc::F_SEAL_SEAL) {
        return Err(io::Error::last_os_error());
    }
    Ok(ReadOnlySegment { mapping, reader })
}

#[cfg(target_os = "linux")]
fn reopen_read_only(fd: RawFd) -> io::Result<OwnedFd> {
    let path = CString::new(format!("/proc/self/fd/{fd}")).map_err(|_| invalid("fd path"))?;
    // SAFETY: open with a NUL-terminated path returns a new descriptor or -1.
    owned(unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) })
}

/// Elsewhere: size the object and open it `O_RDONLY` while its name exists,
/// then map the read-write descriptor and close it.
#[cfg(not(target_os = "linux"))]
fn read_only_segment(kind: SegmentKind, len: usize) -> io::Result<ReadOnlySegment> {
    named_read_only_segment(kind, len).map(|(segment, _)| segment)
}

/// The `shm_open` path of [`create_read_only`], with the name it used (for
/// the test that no name outlives the call).
#[cfg(any(not(target_os = "linux"), test))]
fn named_read_only_segment(kind: SegmentKind, len: usize) -> io::Result<(ReadOnlySegment, String)> {
    let (segment, reader, name) = open_named(kind, |segment, name| {
        truncate(segment, len)?;
        shm_open_cloexec(name, libc::O_RDONLY)
    })?;
    set_cloexec(segment.as_raw_fd())?;
    set_cloexec(reader.as_raw_fd())?;
    let mapping = Mapping::new(fd_of(&segment), len, Protection::ReadWrite)?;
    Ok((ReadOnlySegment { mapping, reader }, name))
}

fn fd_of(segment: &OwnedFd) -> BorrowedFd<'_> {
    use std::os::fd::AsFd;
    segment.as_fd()
}

/// `shm_open` close-on-exec. A system whose `shm_open` refuses the flag
/// (`EINVAL`) gets the descriptor without it; the caller sets `FD_CLOEXEC`
/// right after, and a process another thread spawns between the two calls
/// can inherit it.
#[cfg(any(not(target_os = "linux"), test))]
fn shm_open_cloexec(name: &std::ffi::CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let open = |flags: libc::c_int| {
        // SAFETY: shm_open with a NUL-terminated name; the mode is variadic.
        unsafe { libc::shm_open(name.as_ptr(), flags, 0o600 as libc::c_uint) }
    };
    let fd = open(flags | libc::O_CLOEXEC);
    if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        if flags & libc::O_CREAT != 0 {
            // SAFETY: removes the name in case the refused open made it.
            unsafe { libc::shm_unlink(name.as_ptr()) };
        }
        return owned(open(flags));
    }
    owned(fd)
}

/// A new POSIX shared memory object under a fresh name: `with_name` runs
/// while the name exists, and the name is unlinked before this returns,
/// whatever `with_name` did. Returns the read-write descriptor, what
/// `with_name` returned and the name.
#[cfg(any(not(target_os = "linux"), test))]
fn open_named<T>(
    kind: SegmentKind,
    with_name: impl FnOnce(&OwnedFd, &std::ffi::CStr) -> io::Result<T>,
) -> io::Result<(OwnedFd, T, String)> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let name =
            segment_name(kind, std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed), nanos);
        let c_name = CString::new(name.as_str()).map_err(|_| invalid("segment name"))?;
        let segment = match shm_open_cloexec(&c_name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL) {
            Ok(segment) => segment,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = with_name(&segment, &c_name);
        // SAFETY: removes the name just made; the descriptors keep the object.
        unsafe { libc::shm_unlink(c_name.as_ptr()) };
        return result.map(|value| (segment, value, name));
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free shared memory segment name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn shm_every_segment_name_fits_the_macos_limit() {
        for kind in [SegmentKind::HolderRing, SegmentKind::OutputRing] {
            let longest = segment_name(kind, u32::MAX, u32::MAX, u32::MAX);
            assert_eq!(longest.len(), MAX_NAME_LEN, "{longest}");
            assert!(segment_name(kind, 1, 0, 999_999_999).len() <= MAX_NAME_LEN);
            assert_eq!(longest.matches('/').count(), 1);
            assert!(longest.starts_with('/'));
        }
        // The holder's names are the ones it made before the move.
        assert_eq!(segment_name(SegmentKind::HolderRing, 0x1a2b, 3, 0xff), "/cmhr1a2b.3.ff");
    }

    /// A new segment is close-on-exec from the moment `shm_open` (or
    /// `memfd_create`) returns it, before [`create`] sets the flag again: a
    /// process another daemon thread spawns meanwhile does not inherit a
    /// writable copy of a terminal's output. Red on a macOS major whose
    /// `shm_open` ignores `O_CLOEXEC` (or refuses it: the `EINVAL` fallback
    /// opens without it).
    #[test]
    fn shm_a_new_segment_is_close_on_exec_when_it_is_opened() {
        let named = open_named(SegmentKind::HolderRing, |_, _| Ok(())).unwrap().0;
        for segment in [open_unsized(SegmentKind::HolderRing).unwrap(), named] {
            // SAFETY: F_GETFD of a descriptor this test owns.
            let flags = unsafe { libc::fcntl(segment.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0, "F_GETFD: {}", io::Error::last_os_error());
            assert_ne!(flags & libc::FD_CLOEXEC, 0, "the segment opened without close-on-exec");
        }
    }

    /// No name outlives the call that made it, on the `shm_open` path (the
    /// macOS one, run here on every unix).
    #[test]
    fn shm_no_segment_name_outlives_its_creation() {
        let (segment, name) = named_read_only_segment(SegmentKind::OutputRing, 4096).unwrap();
        assert!(name.len() <= MAX_NAME_LEN);
        let c_name = CString::new(name).unwrap();
        // SAFETY: shm_open with a NUL-terminated name; the mode is variadic.
        let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0) };
        let error = io::Error::last_os_error();
        assert!(fd < 0, "the name outlived the segment");
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
        assert!(!maps_writable(segment.reader.as_fd()), "the named reader maps writable");
        let (_, (), name) = open_named(SegmentKind::HolderRing, |_, _| Ok(())).unwrap();
        let c_name = CString::new(name).unwrap();
        // SAFETY: as above.
        assert!(unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0) } < 0);
    }

    /// The writer's mapping and the reader's descriptor name one segment.
    #[test]
    fn shm_the_reader_sees_what_the_writer_mapping_stores() {
        let segment = create_read_only(SegmentKind::OutputRing, 4096).unwrap();
        // SAFETY: the mapping is 4096 writable bytes.
        unsafe { segment.mapping.as_ptr().add(100).write(0x5a) };
        let view = Mapping::new(segment.reader.as_fd(), 4096, Protection::ReadOnly).unwrap();
        // SAFETY: the view is 4096 readable bytes.
        assert_eq!(unsafe { view.as_ptr().add(100).read() }, 0x5a);
        assert!(!maps_writable(segment.reader.as_fd()));
    }

    #[test]
    fn shm_a_created_segment_is_sized_and_writable() {
        let segment = create(SegmentKind::HolderRing, 8192).unwrap();
        // SAFETY: zeroed stat is a valid out-buffer for fstat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat of a live descriptor into a live local.
        assert_eq!(unsafe { libc::fstat(segment.as_raw_fd(), &mut stat) }, 0);
        assert_eq!(stat.st_size, 8192);
        assert!(maps_writable(segment.as_fd()));
    }
}
