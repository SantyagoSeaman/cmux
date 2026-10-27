//! The broker's link to one `cmux-vt-worker`: frames on one Unix stream
//! socket, a terminal's ring segment passed with `SCM_RIGHTS`.
//!
//! `magic "CMVW" | kind u16 | flags u16 | terminal u64 | len u32 | payload`,
//! little endian, the holder wire's layout (`cmux_holder::wire`) with a
//! 64-bit terminal key. Terminal 0 is the link itself.
//!
//! Daemon to worker, in the order the broker sends them for one terminal:
//! [`Kind::Assign`] (its ring), [`Kind::Seed`] (where to start), then any
//! number of [`Kind::Advance`] and [`Kind::Resize`], and [`Kind::Release`]
//! last. Worker to daemon: [`Kind::Ready`] once, then [`Kind::Parsed`] and
//! [`Kind::Answer`]. A worker answers every Seed with a Parsed at or past
//! the seed's `end`, also when it had nothing to parse: that Parsed is how
//! the broker knows the terminal is restored.
//!
//! Everything a worker sends is untrusted: a bad magic, an unknown kind, an
//! oversized or short payload ends the link, and the broker replaces the
//! worker.

use std::io::{self, Read};

/// Frame magic.
pub const MAGIC: [u8; 4] = *b"CMVW";
/// The wire version the broker speaks; a worker's [`Kind::Ready`] names its
/// own, and a worker of another version is refused.
pub const WIRE_VERSION: u16 = 1;
/// Header bytes.
pub const HEADER: usize = 20;
/// The largest payload either side sends: a prologue (task 210 caps it at
/// 128 KiB) plus the seed's offsets, with room.
pub const MAX_PAYLOAD: usize = 256 * 1024;
/// The largest [`Kind::Answer`] payload: every answer of the fixed set is
/// shorter ([`super::QueryAnswer`]).
pub const MAX_ANSWER: usize = 64;

/// [`Kind::Seed`] flag: output between the prologue's base and
/// `replay_from` is not replayed (it rolled out of the ring, or the strike
/// rule skipped it). The worker cancels any sequence the prologue left open
/// before it parses on, as the in-daemon sink does at a gap.
pub const FLAG_SKIPPED: u16 = 1;

/// Frame kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    /// d->w on terminal 0: `version u16, shard u32`.
    Hello = 1,
    /// d->w: `cols u16, rows u16`, plus one descriptor: the terminal's ring
    /// segment, to map read-only.
    Assign = 2,
    /// d->w: `base u64, replay_from u64, end u64`, then the prologue (a VT
    /// replay of the screen at `base`, may be empty). The worker resets the
    /// terminal, parses the prologue, then the ring from `replay_from` to
    /// at least `end`.
    Seed = 3,
    /// d->w: `end u64`: the ring holds output up to here.
    Advance = 4,
    /// d->w: `cols u16, rows u16`.
    Resize = 5,
    /// d->w: forget the terminal and unmap its ring.
    Release = 6,
    /// w->d on terminal 0: `version u16`.
    Ready = 64,
    /// w->d: `offset u64`: the worker parsed every byte before it.
    Parsed = 65,
    /// w->d: the bytes of one query answer for the terminal's PTY.
    Answer = 66,
}

impl Kind {
    pub fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Assign,
            3 => Self::Seed,
            4 => Self::Advance,
            5 => Self::Resize,
            6 => Self::Release,
            64 => Self::Ready,
            65 => Self::Parsed,
            66 => Self::Answer,
            _ => return None,
        })
    }
}

/// One frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub flags: u16,
    pub terminal: u64,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: Kind, terminal: u64, payload: Vec<u8>) -> Self {
        Self { kind, flags: 0, terminal, payload }
    }

    /// The header and payload as bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.payload.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&(self.kind as u16).to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.terminal.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn hello(shard: u32) -> Self {
        let mut payload = WIRE_VERSION.to_le_bytes().to_vec();
        payload.extend_from_slice(&shard.to_le_bytes());
        Self::new(Kind::Hello, 0, payload)
    }

    pub fn ready(version: u16) -> Self {
        Self::new(Kind::Ready, 0, version.to_le_bytes().to_vec())
    }

    pub fn assign(terminal: u64, cols: u16, rows: u16) -> Self {
        Self::new(Kind::Assign, terminal, size_payload(cols, rows))
    }

    pub fn resize(terminal: u64, cols: u16, rows: u16) -> Self {
        Self::new(Kind::Resize, terminal, size_payload(cols, rows))
    }

    pub fn seed(terminal: u64, seed: &SeedFrame<'_>) -> Self {
        let mut payload = Vec::with_capacity(24 + seed.prologue.len());
        payload.extend_from_slice(&seed.base.to_le_bytes());
        payload.extend_from_slice(&seed.replay_from.to_le_bytes());
        payload.extend_from_slice(&seed.end.to_le_bytes());
        payload.extend_from_slice(seed.prologue);
        let flags = if seed.skipped { FLAG_SKIPPED } else { 0 };
        Self { flags, ..Self::new(Kind::Seed, terminal, payload) }
    }

    pub fn advance(terminal: u64, end: u64) -> Self {
        Self::new(Kind::Advance, terminal, end.to_le_bytes().to_vec())
    }

    pub fn release(terminal: u64) -> Self {
        Self::new(Kind::Release, terminal, Vec::new())
    }

    pub fn parsed(terminal: u64, offset: u64) -> Self {
        Self::new(Kind::Parsed, terminal, offset.to_le_bytes().to_vec())
    }

    pub fn answer(terminal: u64, bytes: &[u8]) -> Self {
        Self::new(Kind::Answer, terminal, bytes.to_vec())
    }
}

fn size_payload(cols: u16, rows: u16) -> Vec<u8> {
    let mut payload = cols.to_le_bytes().to_vec();
    payload.extend_from_slice(&rows.to_le_bytes());
    payload
}

/// A [`Kind::Seed`]'s fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedFrame<'a> {
    pub base: u64,
    pub replay_from: u64,
    pub end: u64,
    pub skipped: bool,
    pub prologue: &'a [u8],
}

impl<'a> SeedFrame<'a> {
    pub fn decode(frame: &'a Frame) -> io::Result<Self> {
        let mut input = Input(&frame.payload);
        Ok(Self {
            base: input.u64()?,
            replay_from: input.u64()?,
            end: input.u64()?,
            skipped: frame.flags & FLAG_SKIPPED != 0,
            prologue: input.0,
        })
    }
}

/// The `u16` of a [`Kind::Hello`] or [`Kind::Ready`].
pub fn decode_version(frame: &Frame) -> io::Result<u16> {
    Input(&frame.payload).u16()
}

/// The `u64` of a [`Kind::Advance`] or [`Kind::Parsed`]; any other length
/// is a bad frame.
pub fn decode_offset(frame: &Frame) -> io::Result<u64> {
    let mut input = Input(&frame.payload);
    let offset = input.u64()?;
    input.end()?;
    Ok(offset)
}

/// `cols, rows` of a [`Kind::Assign`] or [`Kind::Resize`].
pub fn decode_size(frame: &Frame) -> io::Result<(u16, u16)> {
    let mut input = Input(&frame.payload);
    Ok((input.u16()?, input.u16()?))
}

fn bad(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let (head, rest) =
            self.0.split_first_chunk::<N>().ok_or_else(|| bad("short vt worker payload"))?;
        self.0 = rest;
        Ok(*head)
    }

    fn u16(&mut self) -> io::Result<u16> {
        self.take().map(u16::from_le_bytes)
    }

    fn u64(&mut self) -> io::Result<u64> {
        self.take().map(u64::from_le_bytes)
    }

    fn end(&self) -> io::Result<()> {
        if self.0.is_empty() { Ok(()) } else { Err(bad("long vt worker payload")) }
    }
}

/// Reads frames from a stream. Descriptors that arrive are not accepted:
/// a worker sends none, and the daemon's side reads with `read`, so the
/// kernel closes any a worker tried to pass.
pub struct FrameReader<R> {
    stream: R,
    buf: Vec<u8>,
}

impl<R: Read> FrameReader<R> {
    pub fn new(stream: R) -> Self {
        Self { stream, buf: Vec::new() }
    }

    /// The next frame; `Ok(None)` at a clean end of stream (between
    /// frames). A bad magic, an unknown kind, an oversized length or an end
    /// inside a frame is an error.
    pub fn read_frame(&mut self) -> io::Result<Option<Frame>> {
        let mut header = [0u8; HEADER];
        if !self.fill(&mut header)? {
            return Ok(None);
        }
        let [m0, m1, m2, m3, k0, k1, f0, f1, t0, t1, t2, t3, t4, t5, t6, t7, l0, l1, l2, l3] =
            header;
        if [m0, m1, m2, m3] != MAGIC {
            return Err(bad("bad vt worker frame magic"));
        }
        let kind = Kind::from_u16(u16::from_le_bytes([k0, k1]))
            .ok_or_else(|| bad("unknown vt worker frame kind"))?;
        let flags = u16::from_le_bytes([f0, f1]);
        let terminal = u64::from_le_bytes([t0, t1, t2, t3, t4, t5, t6, t7]);
        let len = u32::from_le_bytes([l0, l1, l2, l3]) as usize;
        if len > MAX_PAYLOAD {
            return Err(bad("vt worker frame too large"));
        }
        self.buf.clear();
        self.buf.resize(len, 0);
        if len > 0 && !self.fill_buf()? {
            return Err(bad("vt worker link ended inside a frame"));
        }
        Ok(Some(Frame { kind, flags, terminal, payload: std::mem::take(&mut self.buf) }))
    }

    fn fill(&mut self, header: &mut [u8; HEADER]) -> io::Result<bool> {
        read_all(&mut self.stream, header)
    }

    fn fill_buf(&mut self) -> io::Result<bool> {
        read_all(&mut self.stream, &mut self.buf)
    }
}

/// Fills `buf`: `Ok(false)` when the stream ended before its first byte,
/// an error when it ended after it.
fn read_all(stream: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(bad("vt worker link ended inside a frame")),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(frame: &Frame) -> Frame {
        let bytes = frame.encode();
        FrameReader::new(bytes.as_slice()).read_frame().unwrap().unwrap()
    }

    #[test]
    fn every_frame_round_trips() {
        let seed =
            SeedFrame { base: 7, replay_from: 9, end: 40, skipped: true, prologue: b"\x1b[H" };
        for frame in [
            Frame::hello(3),
            Frame::ready(WIRE_VERSION),
            Frame::assign(5, 80, 24),
            Frame::seed(5, &seed),
            Frame::advance(5, 1 << 40),
            Frame::resize(5, 120, 40),
            Frame::release(5),
            Frame::parsed(5, 99),
            Frame::answer(5, b"\x1b[0n"),
        ] {
            assert_eq!(round_trip(&frame), frame);
        }
        let decoded = round_trip(&Frame::seed(5, &seed));
        assert_eq!(SeedFrame::decode(&decoded).unwrap(), seed);
        assert_eq!(decode_size(&round_trip(&Frame::assign(5, 80, 24))).unwrap(), (80, 24));
        assert_eq!(decode_offset(&round_trip(&Frame::parsed(5, 99))).unwrap(), 99);
        assert_eq!(decode_version(&round_trip(&Frame::hello(1))).unwrap(), WIRE_VERSION);
    }

    #[test]
    fn a_clean_end_is_none_and_a_cut_frame_is_an_error() {
        assert!(FrameReader::new(&[][..]).read_frame().unwrap().is_none());
        let bytes = Frame::parsed(1, 2).encode();
        for cut in 1..bytes.len() {
            assert!(FrameReader::new(&bytes[..cut]).read_frame().is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn untrusted_headers_are_refused() {
        let mut bad_magic = Frame::parsed(1, 2).encode();
        bad_magic[0] = b'X';
        assert!(FrameReader::new(bad_magic.as_slice()).read_frame().is_err());
        let mut unknown = Frame::parsed(1, 2).encode();
        unknown[4..6].copy_from_slice(&999u16.to_le_bytes());
        assert!(FrameReader::new(unknown.as_slice()).read_frame().is_err());
        let mut huge = Frame::parsed(1, 2).encode();
        huge[16..20].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        assert!(FrameReader::new(huge.as_slice()).read_frame().is_err());
    }

    #[test]
    fn offsets_must_be_exactly_eight_bytes() {
        let mut long = Frame::parsed(1, 2);
        long.payload.push(0);
        assert!(decode_offset(&long).is_err());
        let short = Frame::new(Kind::Parsed, 1, vec![0; 7]);
        assert!(decode_offset(&short).is_err());
    }
}
