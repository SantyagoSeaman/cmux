//! A terminal's output ring for the app, `CMAR` layout 1
//! (plans/cmux-next/app-daemon-connection-DESIGN.md, section 4).
//!
//! One writer (the daemon's output path) and any number of readers in other
//! processes, which get a descriptor that cannot write the ring
//! ([`crate::shm::create_read_only`]). The writer never waits for a reader: a
//! reader that falls a capacity behind finds out ([`SpanError::Lapped`]) and
//! resyncs by snapshot.
//!
//! Layout (little endian, [`HEADER`] bytes, then `capacity` data bytes):
//!
//! ```text
//! 0   magic "CMAR"  (4)
//! 4   layout u32 = 1
//! 8   capacity u64
//! 16  base u64       offset of the first byte this ring held
//! 24  end u64        atomic: offset after the last published byte
//! 32  reserve u64    atomic: offset after the last byte the writer has started to write
//! 40  zero to 64
//! 64  data[capacity]; the byte at offset o is at data[o % capacity]
//! ```
//!
//! The writer, per chunk of `n` bytes at `e = end`: `reserve = e + n`, a
//! release fence, the copy (one or two across the wrap), then `end = e + n`
//! (release). A chunk larger than the capacity keeps its last `capacity`
//! bytes. A reader of `[s, t)` reads the bytes, then an acquire fence, then
//! `reserve`: `reserve > s + capacity` means the writer may have overwritten
//! part of what it read. This is the one-writer seqlock of Chromium's
//! `device/base/synchronization/one_writer_seqlock.h`, with the offsets as
//! the sequence: the reader detects every overwrite and the writer never
//! waits. The reader checks when it cuts a span (before anything is queued)
//! and again after it used the bytes ([`OutputRingReader::check`]).

use crate::shm::{self, Mapping, Protection, SegmentKind};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering, fence};

/// Segment magic.
pub const MAGIC: [u8; 4] = *b"CMAR";
/// The layout this build writes and reads.
pub const LAYOUT: u32 = 1;
/// Header bytes before the data.
pub const HEADER: usize = 64;
/// The ring a terminal gets: the holder's per-terminal ring size.
pub const DEFAULT_CAPACITY: usize = 1 << 20;
/// The largest ring a segment may describe: a damaged or hostile header must
/// not make a reader map gigabytes.
pub const MAX_CAPACITY: usize = 64 << 20;

const OFF_LAYOUT: usize = 4;
const OFF_CAPACITY: usize = 8;
const OFF_BASE: usize = 16;
const OFF_END: usize = 24;
const OFF_RESERVE: usize = 32;

/// A ring that cannot be made or opened. Each ends the stream that carried
/// it (a protocol error, never a silent fallback).
#[derive(Debug)]
pub enum RingError {
    /// The segment could not be made, sized, mapped or inspected.
    Io(io::Error),
    /// A capacity of 0 or above [`MAX_CAPACITY`].
    BadCapacity { capacity: u64 },
    /// The segment is smaller than its header and capacity say.
    TooSmall { size: u64, needed: u64 },
    /// The segment's magic is not `CMAR`.
    BadMagic,
    /// A layout this build does not read.
    BadLayout { layout: u32 },
    /// The header's end is before its base.
    EndBeforeBase { base: u64, end: u64 },
    /// The descriptor maps writable: a reader holding it could change the
    /// ring for every other reader.
    Writable,
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "output ring segment: {error}"),
            Self::BadCapacity { capacity } => {
                write!(f, "output ring capacity {capacity} is not in 1..={MAX_CAPACITY}")
            }
            Self::TooSmall { size, needed } => {
                write!(f, "output ring segment of {size} bytes, needs {needed}")
            }
            Self::BadMagic => f.write_str("not an output ring segment"),
            Self::BadLayout { layout } => write!(f, "output ring layout {layout} is not {LAYOUT}"),
            Self::EndBeforeBase { base, end } => {
                write!(f, "output ring ends at {end}, before its base {base}")
            }
            Self::Writable => f.write_str("output ring descriptor maps writable"),
        }
    }
}

impl std::error::Error for RingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for RingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Why a reader may not use `[from, to)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanError {
    /// The writer has overwritten, or may be overwriting, part of the span:
    /// it is more than a capacity long, or the writer reserved past
    /// `from + capacity`. The stream resyncs.
    Lapped { from: u64, reserve: u64 },
    /// The span ends past the last published byte.
    NotPublished { to: u64, end: u64 },
    /// The span starts before the first byte this ring held.
    BeforeBase { from: u64, base: u64 },
    /// The span ends before it starts.
    Backwards { from: u64, to: u64 },
}

impl fmt::Display for SpanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lapped { from, reserve } => {
                write!(f, "output ring lapped the span at {from} (writer reserved to {reserve})")
            }
            Self::NotPublished { to, end } => {
                write!(f, "output ring span ends at {to}, past the published end {end}")
            }
            Self::BeforeBase { from, base } => {
                write!(f, "output ring span starts at {from}, before the ring's base {base}")
            }
            Self::Backwards { from, to } => write!(f, "output ring span {from}..{to} is backwards"),
        }
    }
}

impl std::error::Error for SpanError {}

fn check_capacity(capacity: u64) -> Result<(), RingError> {
    if capacity == 0 || capacity > MAX_CAPACITY as u64 {
        return Err(RingError::BadCapacity { capacity });
    }
    Ok(())
}

/// The header word at `offset` of a mapping at least [`HEADER`] bytes long.
///
/// # Safety
/// `map` is the start of a live, page-aligned mapping of at least
/// [`HEADER`] bytes, and `offset` is 8-byte aligned and below `HEADER - 8`.
unsafe fn word<'a>(map: *mut u8, offset: usize) -> &'a AtomicU64 {
    // SAFETY: the caller's contract: an aligned u64 inside the mapping;
    // AtomicU64 has u64's layout.
    unsafe { &*map.add(offset).cast::<AtomicU64>() }
}

/// The ring's only writer: the daemon's output path for one terminal.
#[derive(Debug)]
pub struct OutputRingWriter {
    mapping: Mapping,
    /// The read-only descriptor; every reader gets a copy.
    reader: OwnedFd,
    capacity: u64,
    base: u64,
    end: u64,
}

impl OutputRingWriter {
    /// An empty ring of `capacity` bytes whose next byte has offset `base`.
    pub fn create(base: u64, capacity: usize) -> Result<Self, RingError> {
        check_capacity(capacity as u64)?;
        let segment = shm::create_read_only(SegmentKind::OutputRing, HEADER + capacity)?;
        let map = segment.mapping.as_ptr();
        // SAFETY: the mapping is HEADER + capacity writable bytes and no
        // reader has the segment yet; the words are aligned in the header.
        unsafe {
            let header = std::slice::from_raw_parts_mut(map, HEADER);
            header.fill(0);
            if let Some(magic) = header.get_mut(..4) {
                magic.copy_from_slice(&MAGIC);
            }
            if let Some(layout) = header.get_mut(OFF_LAYOUT..OFF_LAYOUT + 4) {
                layout.copy_from_slice(&LAYOUT.to_le_bytes());
            }
            word(map, OFF_CAPACITY).store(capacity as u64, Ordering::Relaxed);
            word(map, OFF_BASE).store(base, Ordering::Relaxed);
            word(map, OFF_RESERVE).store(base, Ordering::Relaxed);
            word(map, OFF_END).store(base, Ordering::Release);
        }
        Ok(Self {
            mapping: segment.mapping,
            reader: segment.reader,
            capacity: capacity as u64,
            base,
            end: base,
        })
    }

    /// A read-only descriptor of the ring for one reader (close-on-exec).
    pub fn reader_fd(&self) -> io::Result<OwnedFd> {
        self.reader.try_clone()
    }

    /// The read-only descriptor every [`Self::reader_fd`] copies.
    pub fn reader(&self) -> BorrowedFd<'_> {
        self.reader.as_fd()
    }

    /// Ring bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Offset of the first byte this ring held.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Offset after the last published byte.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Append `bytes` and publish them. Never waits; a chunk larger than the
    /// capacity keeps its last `capacity` bytes (the offsets before them are
    /// skipped, and a reader of them is lapped).
    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let capacity = self.capacity as usize;
        let end = self.end + bytes.len() as u64;
        let kept = bytes.get(bytes.len().saturating_sub(capacity)..).unwrap_or(bytes);
        let start = end - kept.len() as u64;
        let map = self.mapping.as_ptr();
        // SAFETY: the header words are aligned in the mapping; the data copy
        // stays inside `HEADER .. HEADER + capacity` (`at < capacity`,
        // `first <= capacity - at`, `kept.len() <= capacity`). Readers only
        // read the data area, through their own mappings.
        unsafe {
            word(map, OFF_RESERVE).store(end, Ordering::Relaxed);
            fence(Ordering::Release);
            let at = (start % self.capacity) as usize;
            let first = kept.len().min(capacity - at);
            let data = map.add(HEADER);
            std::ptr::copy_nonoverlapping(kept.as_ptr(), data.add(at), first);
            std::ptr::copy_nonoverlapping(kept.as_ptr().add(first), data, kept.len() - first);
            word(map, OFF_END).store(end, Ordering::Release);
        }
        self.end = end;
    }
}

/// A reader's view of a ring another process writes, mapped read-only.
#[derive(Debug)]
pub struct OutputRingReader {
    mapping: Mapping,
    capacity: u64,
    base: u64,
}

impl OutputRingReader {
    /// Map the ring `fd` names, read-only. Refuses a descriptor that maps
    /// writable and a header that is not `CMAR` layout 1 with a capacity in
    /// `1..=MAX_CAPACITY` that the segment holds. The caller may close `fd`
    /// afterwards: the mapping keeps the segment.
    pub fn open(fd: BorrowedFd<'_>) -> Result<Self, RingError> {
        if shm::maps_writable(fd) {
            return Err(RingError::Writable);
        }
        // SAFETY: zeroed stat is a valid out-buffer for fstat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat of a live descriptor into a live local.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // macOS rounds a `shm_open` object up to whole pages: the size only
        // bounds what the header may claim, the header says what to map.
        let size = u64::try_from(stat.st_size).unwrap_or(0);
        let too_small = |needed: u64| RingError::TooSmall { size, needed };
        if size < HEADER as u64 {
            return Err(too_small(HEADER as u64));
        }
        let header = Mapping::new(fd, HEADER, Protection::ReadOnly)?;
        let map = header.as_ptr();
        // SAFETY: the mapping is HEADER readable bytes.
        let bytes = unsafe { std::slice::from_raw_parts(map.cast_const(), HEADER) };
        let field = |at: usize| -> [u8; 4] {
            bytes.get(at..at + 4).and_then(|bytes| bytes.try_into().ok()).unwrap_or_default()
        };
        if field(0) != MAGIC {
            return Err(RingError::BadMagic);
        }
        let layout = u32::from_le_bytes(field(OFF_LAYOUT));
        if layout != LAYOUT {
            return Err(RingError::BadLayout { layout });
        }
        // SAFETY: header words of a mapping of HEADER bytes.
        let (capacity, base, end) = unsafe {
            (
                word(map, OFF_CAPACITY).load(Ordering::Relaxed),
                word(map, OFF_BASE).load(Ordering::Relaxed),
                word(map, OFF_END).load(Ordering::Acquire),
            )
        };
        check_capacity(capacity)?;
        if HEADER as u64 + capacity > size {
            return Err(too_small(HEADER as u64 + capacity));
        }
        if end < base {
            return Err(RingError::EndBeforeBase { base, end });
        }
        // `check_capacity` bounded it by MAX_CAPACITY, so it fits a usize.
        let mapping = Mapping::new(fd, HEADER + capacity as usize, Protection::ReadOnly)?;
        Ok(Self { mapping, capacity, base })
    }

    /// Ring bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Offset of the first byte this ring held.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Offset after the last published byte.
    pub fn end(&self) -> u64 {
        // SAFETY: a header word of this value's mapping.
        unsafe { word(self.mapping.as_ptr(), OFF_END).load(Ordering::Acquire) }
    }

    fn reserve(&self) -> u64 {
        // SAFETY: a header word of this value's mapping.
        unsafe { word(self.mapping.as_ptr(), OFF_RESERVE).load(Ordering::Relaxed) }
    }

    fn lapped(&self, from: u64) -> Result<(), SpanError> {
        let reserve = self.reserve();
        if reserve > from.saturating_add(self.capacity) {
            return Err(SpanError::Lapped { from, reserve });
        }
        Ok(())
    }

    /// Whether `[from, to)` may be cut: published, inside the ring, and not
    /// being overwritten. Run before the span's bytes are handed on.
    pub fn cut(&self, from: u64, to: u64) -> Result<(), SpanError> {
        if to < from {
            return Err(SpanError::Backwards { from, to });
        }
        if from < self.base {
            return Err(SpanError::BeforeBase { from, base: self.base });
        }
        let end = self.end();
        if to > end {
            return Err(SpanError::NotPublished { to, end });
        }
        if to - from > self.capacity {
            return Err(SpanError::Lapped { from, reserve: self.reserve() });
        }
        self.lapped(from)
    }

    /// After the bytes of a span starting at `from` were used: whether the
    /// writer overwrote any of them meanwhile.
    pub fn check(&self, from: u64) -> Result<(), SpanError> {
        fence(Ordering::Acquire);
        self.lapped(from)
    }

    /// Append the bytes of `[from, to)` to `out`: [`Self::cut`], the copy,
    /// then [`Self::check`]. On a lap, `out` is as it was.
    pub fn read_into(&self, from: u64, to: u64, out: &mut Vec<u8>) -> Result<(), SpanError> {
        self.cut(from, to)?;
        let len = (to - from) as usize;
        let at = (from % self.capacity) as usize;
        let first = len.min(self.capacity as usize - at);
        let kept = out.len();
        out.reserve(len);
        // SAFETY: `cut` bounded `len` by the capacity, so both copies stay in
        // the data area of the mapping (`HEADER + capacity` bytes, checked
        // at open), and `out` has room for `len` more bytes.
        unsafe {
            let data = self.mapping.as_ptr().cast_const().add(HEADER);
            let dst = out.as_mut_ptr().add(kept);
            std::ptr::copy_nonoverlapping(data.add(at), dst, first);
            std::ptr::copy_nonoverlapping(data, dst.add(first), len - first);
            out.set_len(kept + len);
        }
        if let Err(error) = self.check(from) {
            out.truncate(kept);
            return Err(error);
        }
        Ok(())
    }
}
