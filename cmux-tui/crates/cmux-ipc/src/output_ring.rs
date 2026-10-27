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
//! every reading cursor ([`Publish::gapped`]). A gapped reader cannot cut
//! across the hole ([`SpanError::BeforeBase`]); its ack at or past the hole
//! ([`RingAck::GapAcked`]) says it holds nothing more. So a span a reader
//! holds is never overwritten: a slow reader costs a resync, never torn bytes.
//!
//! One stall, one resync per reader. The skips of a stall last while any
//! gapped cursor has not acked its gap; a cursor that acked waits, holding
//! nothing and blocking nothing, and is not gapped again. When the last gap
//! is acked the stall is over, and every waiting cursor is released together
//! ([`Publish::released`], [`RingAcked::released`]): it reads again from the
//! ring's end, where its stream resumes at a snapshot taken after the release.
//! A reader that opens during a stall waits for its end the same way.
//!
//! The ring is shared by every viewer of a terminal, so a skip costs them all
//! the skipped bytes. A gapped cursor therefore holds the stall for
//! [`GAP_HOLD_CAPACITIES`] capacities of output since its gap, no longer:
//! time for its reader to finish the span it parses. A reader that has not
//! acked by then has stopped (a hung app) or does not ack. Its cursor lapses
//! and is released: the writer no longer honors it, its stream resyncs, and
//! a span it still holds is found lapped by its release check
//! ([`OutputRingReader::check`]), as before cursors existed. Its next ack
//! that leaves the writer a capacity of room makes it a reading cursor again.

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

/// Where one reader's cursor stands with the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    /// It reads the ring and holds no span below `acked`.
    Reading,
    /// A skip left a hole that ends at `gap`; the reader may still hold
    /// spans at or after `acked`. `since` counts the output pushed since the
    /// gap, against [`GAP_HOLD_CAPACITIES`].
    Gapped { gap: u64, since: u64 },
    /// It acked its gap (or opened during a stall): it holds nothing and
    /// blocks nothing, and is released when the stall is over.
    Waiting,
    /// It did not ack its gap within the hold: the writer no longer honors
    /// it, until an ack leaves the writer a capacity of room.
    Lapsed,
}

/// The writer's view of one reader's cursor.
#[derive(Debug)]
struct Cursor {
    id: u64,
    live: Weak<ReaderToken>,
    /// The reader holds no span below this offset and cuts none below it.
    acked: u64,
    standing: Standing,
}

impl Cursor {
    /// Whether writing up to `end` could overwrite a published byte at or
    /// after `acked` while the ring ends at `published`: the reader may hold
    /// such a byte, and the byte at `o` shares its slot with `o + capacity`.
    /// A waiting or lapsed cursor holds nothing the writer honors.
    fn blocks(&self, published: u64, end: u64, capacity: u64) -> bool {
        matches!(self.standing, Standing::Reading | Standing::Gapped { .. })
            && self.acked < published
            && end > self.acked.saturating_add(capacity)
    }

    fn gapped(&self) -> bool {
        matches!(self.standing, Standing::Gapped { .. })
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
    /// cut fails, and its stream resyncs once the cursor is released.
    pub gapped: Vec<u64>,
    /// Cursors this push released: the stall ended (the last gapped cursor
    /// lapsed) or the cursor itself lapsed. Each stream resyncs now, by a
    /// snapshot taken after this push.
    pub released: Vec<u64>,
}

/// What a RING_ACK did to its cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingAck {
    /// The cursor moved to the acked offset (a lapsed cursor reads again).
    Moved,
    /// The cursor was at or past the offset already (an ack may arrive late),
    /// or holds nothing the offset could move.
    Stale,
    /// The reader acked its gap: it holds nothing. Its stream resyncs when
    /// [`RingAcked::released`] names it, now or when the stall ends.
    GapAcked,
    /// No open cursor of this writer.
    Unknown,
}

/// What [`OutputRingWriter::ack`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingAcked {
    /// What the ack did to its own cursor.
    pub ack: RingAck,
    /// Cursors the ack released (it acked the stall's last gap), the acking
    /// one included: each stream resyncs now, by a snapshot taken after this
    /// ack.
    pub released: Vec<u64>,
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

    /// Whether a stall is on: a gapped cursor has not acked its gap yet. A
    /// reader that opens now waits for its end ([`Self::open_reader`]).
    pub fn stalled(&self) -> bool {
        self.cursors.iter().any(|cursor| cursor.gapped() && cursor.live.strong_count() > 0)
    }

    /// A consumer cursor for one more reader, acked at the ring's end now:
    /// the reader's first span starts at or after it. During a stall
    /// ([`Self::stalled`]) the cursor waits: the stall's skips would gap it
    /// anyway, so its stream's first snapshot waits for the stall's end.
    pub fn open_reader(&mut self) -> RingReader {
        let id = self.next_reader;
        self.next_reader += 1;
        let token = Arc::new(ReaderToken { id });
        let standing = if self.stalled() { Standing::Waiting } else { Standing::Reading };
        self.cursors.push(Cursor { id, live: Arc::downgrade(&token), acked: self.end, standing });
        RingReader { token }
    }

    /// `reader` acked `offset` (RING_ACK): it holds no span below it and cuts
    /// none below it. An offset past the end counts as the end.
    pub fn ack(&mut self, reader: &RingReader, offset: u64) -> RingAcked {
        let (end, capacity) = (self.end, self.capacity);
        let token = Arc::as_ptr(&reader.token);
        let Some(cursor) = self.cursors.iter_mut().find(|cursor| cursor.live.as_ptr() == token)
        else {
            return RingAcked { ack: RingAck::Unknown, released: Vec::new() };
        };
        let offset = offset.min(end);
        let ack = match cursor.standing {
            Standing::Gapped { gap, .. } if offset >= gap => {
                cursor.standing = Standing::Waiting;
                RingAck::GapAcked
            }
            Standing::Reading | Standing::Gapped { .. } if offset > cursor.acked => {
                cursor.acked = offset;
                RingAck::Moved
            }
            Standing::Lapsed if offset.saturating_add(capacity) >= end => {
                cursor.standing = Standing::Reading;
                cursor.acked = offset;
                RingAck::Moved
            }
            Standing::Reading | Standing::Gapped { .. } | Standing::Waiting | Standing::Lapsed => {
                RingAck::Stale
            }
        };
        let released = if ack == RingAck::GapAcked { self.end_stall() } else { Vec::new() };
        RingAcked { ack, released }
    }

    /// Append `bytes` and publish them. Never waits. A chunk that could
    /// overwrite a byte an open cursor may still hold is skipped instead (see
    /// the module docs); otherwise a chunk larger than the capacity keeps its
    /// last `capacity` bytes (the offsets before them are lost, and a reader
    /// of them is lapped).
    pub fn push(&mut self, bytes: &[u8]) -> Publish {
        let len = bytes.len() as u64;
        let end = self.end + len;
        if bytes.is_empty() {
            return Publish { end, skipped: false, gapped: Vec::new(), released: Vec::new() };
        }
        self.cursors.retain(|cursor| cursor.live.strong_count() > 0);
        let (published, capacity) = (self.end, self.capacity);
        let skipped = self.cursors.iter().any(|cursor| cursor.blocks(published, end, capacity));
        if skipped {
            self.skip(end, len);
        } else {
            self.write(end, bytes);
        }
        let mut gapped = Vec::new();
        let mut released = Vec::new();
        let hold = capacity.saturating_mul(GAP_HOLD_CAPACITIES);
        for cursor in &mut self.cursors {
            match cursor.standing {
                Standing::Reading if skipped => {
                    cursor.standing = Standing::Gapped { gap: end, since: 0 };
                    gapped.push(cursor.id);
                }
                Standing::Gapped { gap, since } => {
                    let since = since.saturating_add(len);
                    if since >= hold {
                        cursor.standing = Standing::Lapsed;
                        released.push(cursor.id);
                    } else {
                        cursor.standing = Standing::Gapped { gap, since };
                    }
                }
                Standing::Reading | Standing::Waiting | Standing::Lapsed => {}
            }
        }
        released.extend(self.end_stall());
        Publish { end, skipped, gapped, released }
    }

    /// When no gapped cursor is left, the stall is over: every waiting cursor
    /// reads again from the ring's end. The ids it released.
    fn end_stall(&mut self) -> Vec<u64> {
        if self.stalled() {
            return Vec::new();
        }
        let end = self.end;
        let mut released = Vec::new();
        for cursor in &mut self.cursors {
            if cursor.standing == Standing::Waiting && cursor.live.strong_count() > 0 {
                cursor.standing = Standing::Reading;
                cursor.acked = end;
                released.push(cursor.id);
            }
        }
        released
    }

    /// Moves the ring past a chunk without writing it: `base` first, then
    /// `end` (release), so a reader that sees the new end sees the hole.
    fn skip(&mut self, end: u64, len: u64) {
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
