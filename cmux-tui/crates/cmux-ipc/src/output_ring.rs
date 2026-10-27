//! A terminal's output ring for the app, `CMAR` layout 1
//! (plans/cmux-next/app-daemon-connection-DESIGN.md, section 4).
//!
//! One writer (the daemon's output path) and any number of readers in other
//! processes, which get a descriptor that cannot write the ring
//! ([`crate::shm::create_read_only`]). The writer never waits for a reader: a
//! reader that falls a capacity behind finds out ([`SpanError::Lapped`]) and
//! resyncs by snapshot.
//!
//! Consumer cursors (RING_ACK, p05-ipc-119). A reader that parses spans in
//! place has a [`RingReader`] cursor in the writer: the offset it acked, below
//! which it holds no span and cuts none. The writer never writes past a
//! cursor's acked offset plus the capacity, the bound a Mojo data pipe's
//! producer has against its consumer's read position
//! (mojo/core/data_pipe_producer_dispatcher.cc, `available_capacity_`). It
//! does not wait either: it skips the chunk, moves `base` past it and gaps
//! every cursor ([`Publish::gapped`]). A gapped reader cannot cut across the
//! hole ([`SpanError::BeforeBase`]); its ack at or past the hole
//! ([`RingAck::GapReleased`]) says it holds nothing more, and it resumes at a
//! snapshot taken after that ack. So a span a reader holds is never
//! overwritten: a slow reader costs one resync, never torn bytes.
//!
//! The ring is shared by every viewer of a terminal, so a skip costs them all
//! the skipped bytes. A gapped cursor therefore holds the writer back only
//! for [`GAP_HOLD_CAPACITIES`] capacities of skipped output: time for its
//! reader to finish the span it parses. A reader still holding a span after
//! that has stopped (a hung app); the writer writes again, and that reader's
//! release check finds the lap ([`OutputRingReader::check`]), as before
//! cursors existed.
//!
//! Layout (little endian, [`HEADER`] bytes, then `capacity` data bytes):
//!
//! ```text
//! 0   magic "CMAR"  (4)
//! 4   layout u32 = 1
//! 8   capacity u64
//! 16  base u64       atomic: offset of the first byte the ring holds; moves
//!                    forward only when the writer skips a chunk (a gap)
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
//! part of what it read. A skipped chunk stores `base` past it before it
//! publishes `end`, so a reader that sees the new end also sees the new base
//! and never cuts the hole. This is the one-writer seqlock of Chromium's
//! `device/base/synchronization/one_writer_seqlock.h`, with the offsets as
//! the sequence: the reader detects every overwrite and the writer never
//! waits. The reader checks when it cuts a span (before anything is queued)
//! and again after it used the bytes ([`OutputRingReader::check`]).

use crate::shm::{self, Mapping, Protection, SegmentKind};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering, fence};
use std::sync::{Arc, Weak};

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
/// How much output, in capacities, the writer skips for one gapped cursor
/// before it stops honoring it (see the module docs).
pub const GAP_HOLD_CAPACITIES: u64 = 2;

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
    /// The span starts before the first byte the ring holds: the ring never
    /// held it, or the writer skipped it for a reader's cursor (a gap).
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

/// One reader's consumer cursor, held by whoever routes that reader's
/// RING_ACKs. Dropping it closes the cursor: the writer stops honoring it.
#[derive(Debug)]
pub struct RingReader {
    token: Arc<ReaderToken>,
}

#[derive(Debug)]
struct ReaderToken {
    id: u64,
}

impl RingReader {
    /// The cursor's id in its writer ([`Publish::gapped`]).
    pub fn id(&self) -> u64 {
        self.token.id
    }
}

/// The writer's view of one reader's cursor.
#[derive(Debug)]
struct Cursor {
    id: u64,
    live: Weak<ReaderToken>,
    /// The reader holds no span below this offset and cuts none below it.
    acked: u64,
    /// Gapped: the base after the skipped chunk, which the reader acks once
    /// it holds nothing more.
    gap: Option<u64>,
    /// Output skipped since the gap.
    skipped_since_gap: u64,
}

impl Cursor {
    /// Whether writing up to `end` could overwrite a published byte at or
    /// after `acked` while the ring ends at `published`: the reader may hold
    /// such a byte, and the byte at `o` shares its slot with `o + capacity`.
    /// A gapped cursor stops holding the writer after
    /// [`GAP_HOLD_CAPACITIES`] capacities of skipped output.
    fn blocks(&self, published: u64, end: u64, capacity: u64) -> bool {
        let held_too_long = self.gap.is_some()
            && self.skipped_since_gap >= capacity.saturating_mul(GAP_HOLD_CAPACITIES);
        !held_too_long && self.acked < published && end > self.acked.saturating_add(capacity)
    }
}

/// What one [`OutputRingWriter::push`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publish {
    /// Offset after the chunk: the ring's end now, written or skipped.
    pub end: u64,
    /// The chunk was skipped for a cursor: its bytes are not in the ring and
    /// `base` moved to `end`.
    pub skipped: bool,
    /// Cursors this push gapped (by [`RingReader::id`]): each reader's next
    /// cut fails, and it resyncs after it acks the gap.
    pub gapped: Vec<u64>,
}

/// What a RING_ACK did to its cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingAck {
    /// The cursor moved to the acked offset.
    Moved,
    /// The cursor was at or past the offset already (an ack may arrive late).
    Stale,
    /// The reader acked its gap: it holds nothing, so its cursor moved to the
    /// ring's end. Its stream resyncs now, by a snapshot taken after this ack
    /// (at or after that end).
    GapReleased,
    /// No open cursor of this writer.
    Unknown,
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
    cursors: Vec<Cursor>,
    next_reader: u64,
    skips: u64,
    skipped_bytes: u64,
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
            cursors: Vec::new(),
            next_reader: 0,
            skips: 0,
            skipped_bytes: 0,
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

    /// Offset of the first byte the ring holds.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Offset after the last published byte.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Chunks skipped for a cursor since the ring was made.
    pub fn skips(&self) -> u64 {
        self.skips
    }

    /// Bytes those chunks held.
    pub fn skipped_bytes(&self) -> u64 {
        self.skipped_bytes
    }

    /// A consumer cursor for one more reader, acked at the ring's end now:
    /// the reader's first span starts at or after it.
    pub fn open_reader(&mut self) -> RingReader {
        let id = self.next_reader;
        self.next_reader += 1;
        let token = Arc::new(ReaderToken { id });
        self.cursors.push(Cursor {
            id,
            live: Arc::downgrade(&token),
            acked: self.end,
            gap: None,
            skipped_since_gap: 0,
        });
        RingReader { token }
    }

    /// `reader` acked `offset` (RING_ACK): it holds no span below it and cuts
    /// none below it. An offset past the end counts as the end.
    pub fn ack(&mut self, reader: &RingReader, offset: u64) -> RingAck {
        let end = self.end;
        let token = Arc::as_ptr(&reader.token);
        let Some(cursor) = self.cursors.iter_mut().find(|cursor| cursor.live.as_ptr() == token)
        else {
            return RingAck::Unknown;
        };
        let offset = offset.min(end);
        if cursor.gap.is_some_and(|gap| offset >= gap) {
            cursor.gap = None;
            cursor.skipped_since_gap = 0;
            cursor.acked = end;
            return RingAck::GapReleased;
        }
        if offset <= cursor.acked {
            return RingAck::Stale;
        }
        cursor.acked = offset;
        RingAck::Moved
    }

    /// Append `bytes` and publish them. Never waits. A chunk that could
    /// overwrite a byte an open cursor may still hold is skipped instead (see
    /// the module docs); otherwise a chunk larger than the capacity keeps its
    /// last `capacity` bytes (the offsets before them are lost, and a reader
    /// of them is lapped).
    pub fn push(&mut self, bytes: &[u8]) -> Publish {
        let end = self.end + bytes.len() as u64;
        if bytes.is_empty() {
            return Publish { end, skipped: false, gapped: Vec::new() };
        }
        self.cursors.retain(|cursor| cursor.live.strong_count() > 0);
        let (published, capacity) = (self.end, self.capacity);
        if self.cursors.iter().any(|cursor| cursor.blocks(published, end, capacity)) {
            return self.skip(end, bytes.len() as u64);
        }
        self.write(end, bytes);
        Publish { end, skipped: false, gapped: Vec::new() }
    }

    /// Moves the ring past a chunk without writing it: `base` first, then
    /// `end` (release), so a reader that sees the new end sees the hole.
    /// Every cursor not gapped yet is gapped at the new base; every gapped
    /// one counts the skipped bytes against its hold.
    fn skip(&mut self, end: u64, len: u64) -> Publish {
        let map = self.mapping.as_ptr();
        // SAFETY: header words of the writer's mapping.
        unsafe {
            word(map, OFF_BASE).store(end, Ordering::Relaxed);
            word(map, OFF_END).store(end, Ordering::Release);
        }
        self.base = end;
        self.end = end;
        self.skips += 1;
        self.skipped_bytes += len;
        let mut gapped = Vec::new();
        for cursor in &mut self.cursors {
            if cursor.gap.is_none() {
                cursor.gap = Some(end);
                gapped.push(cursor.id);
            }
            cursor.skipped_since_gap = cursor.skipped_since_gap.saturating_add(len);
        }
        Publish { end, skipped: true, gapped }
    }

    /// The copy of one chunk that ends at `end`, then its publication.
    fn write(&mut self, end: u64, bytes: &[u8]) {
        let capacity = self.capacity as usize;
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
        Ok(Self { mapping, capacity })
    }

    /// Ring bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Offset of the first byte the ring holds. Read after [`Self::end`], it
    /// is at least the base of every skip that end follows.
    pub fn base(&self) -> u64 {
        // SAFETY: a header word of this value's mapping.
        unsafe { word(self.mapping.as_ptr(), OFF_BASE).load(Ordering::Relaxed) }
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
        let end = self.end();
        let base = self.base();
        if from < base {
            return Err(SpanError::BeforeBase { from, base });
        }
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
