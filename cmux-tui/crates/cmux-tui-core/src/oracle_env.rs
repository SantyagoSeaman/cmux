//! The owner's wall clock and id source, as one value the owner start builds
//! and passes down (plans/cmux-next/chrome-parity/p15-oracle.md, task 185).
//!
//! A store that stamps a time or mints an id takes them from the
//! [`OracleEnv`] it was given, never from `SystemTime::now()` or the system's
//! random source directly. In production the value is the system clock and
//! the system's random source, so those stores behave as they did. Under the
//! correctness oracle (debug and `--features oracle` builds with
//! `CMUX_TUI_ORACLE_SEED`, `crate::switches`) the clock is virtual and the ids
//! come from a ChaCha20 stream keyed by the seed, so two daemons that run the
//! same op script write the same ids and times, byte for byte.
//!
//! This is Chromium's time override and virtual time:
//! `base/time/time_override.h` (`ScopedTimeClockOverrides` replaces
//! `Time::Now` for a test) and the DevTools `Emulation.setVirtualTimePolicy`
//! of headless runs, where time moves only when the test lets it. The virtual
//! clock here starts at the fixture epoch and moves only on
//! `oracle-advance-clock`. The seeded stream is the ChaCha20 block function
//! (RFC 8439) over a block counter, written here so no crate is added. An
//! owner that restarts on a persistent root continues the clock and takes a
//! fresh stream ([`OracleEnv::resume_in`], `oracle_env/record.rs`), so it
//! never mints an id or a time of an earlier launch again.

#[cfg(any(debug_assertions, feature = "oracle"))]
mod record;

use std::path::Path;
#[cfg(any(debug_assertions, feature = "oracle"))]
use std::sync::Arc;
#[cfg(any(debug_assertions, feature = "oracle"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(debug_assertions, feature = "oracle"))]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

/// The clock and the id source of one owner. Cloning shares them: a store
/// opened with a clone sees the same virtual time and draws from the same
/// id stream.
#[derive(Clone, Debug, Default)]
pub struct OracleEnv {
    wall: WallClock,
    ids: IdSource,
}

impl OracleEnv {
    /// The virtual clock at the fixture epoch and the id stream of `seed`.
    #[cfg(any(debug_assertions, feature = "oracle"))]
    pub(crate) fn seeded(seed: &str) -> Self {
        Self {
            wall: WallClock::Virtual(Arc::new(VirtualClock::default())),
            ids: IdSource::Seeded(Arc::new(SeededIds::new(seed))),
        }
    }

    /// This env as the owner of the session state `directory` resumes it:
    /// a seeded env continues the clock of the launches before it on that
    /// directory and draws from a stream of its own; the system env and an
    /// in-memory session (`None`) are unchanged. Call it once, at the owner
    /// open, before any store takes a clone.
    #[cfg_attr(not(any(debug_assertions, feature = "oracle")), allow(unused_variables))]
    pub(crate) fn resume_in(self, directory: Option<&Path>) -> std::io::Result<Self> {
        #[cfg(any(debug_assertions, feature = "oracle"))]
        if let (Some(directory), IdSource::Seeded(ids), WallClock::Virtual(clock)) =
            (directory, &self.ids, &self.wall)
        {
            let (file, launch) = record::RecordFile::open(directory)?;
            let offset_ms = launch.clock_offset_ms.max(clock.offset_ms());
            return Ok(Self {
                wall: WallClock::Virtual(Arc::new(VirtualClock {
                    offset_ms: AtomicU64::new(offset_ms),
                    record: Some(file),
                })),
                ids: IdSource::Seeded(Arc::new(ids.stream(launch.launches))),
            });
        }
        Ok(self)
    }

    pub(crate) fn wall(&self) -> &WallClock {
        &self.wall
    }

    pub(crate) fn ids(&self) -> &IdSource {
        &self.ids
    }

    /// The virtual clock `oracle-advance-clock` moves; `None` on the system
    /// clock.
    #[cfg(any(debug_assertions, feature = "oracle"))]
    pub(crate) fn virtual_clock(&self) -> Option<&VirtualClock> {
        match &self.wall {
            WallClock::System => None,
            WallClock::Virtual(clock) => Some(clock),
        }
    }
}

/// Where a store's "now" comes from.
#[derive(Clone, Debug, Default)]
pub(crate) enum WallClock {
    #[default]
    System,
    #[cfg(any(debug_assertions, feature = "oracle"))]
    Virtual(Arc<VirtualClock>),
}

impl WallClock {
    pub(crate) fn now(&self) -> SystemTime {
        match self {
            Self::System => SystemTime::now(),
            #[cfg(any(debug_assertions, feature = "oracle"))]
            Self::Virtual(clock) => clock.now(),
        }
    }

    /// Milliseconds since the Unix epoch; 0 before it.
    pub(crate) fn unix_ms(&self) -> u64 {
        self.try_unix_ms().unwrap_or_else(|error| match error {
            ClockError::BeforeUnixEpoch => 0,
            ClockError::OutOfRange => u64::MAX,
            #[cfg(any(debug_assertions, feature = "oracle"))]
            ClockError::RecordWrite(_) => u64::MAX,
        })
    }

    /// Milliseconds since the Unix epoch, refusing a clock before it.
    pub(crate) fn try_unix_ms(&self) -> Result<u64, ClockError> {
        let elapsed =
            self.now().duration_since(UNIX_EPOCH).map_err(|_| ClockError::BeforeUnixEpoch)?;
        u64::try_from(elapsed.as_millis()).map_err(|_| ClockError::OutOfRange)
    }
}

/// A wall clock reading a store cannot use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClockError {
    BeforeUnixEpoch,
    OutOfRange,
    /// The clock moved, but its launch record could not be written, so a
    /// restart on this root would resume it earlier.
    #[cfg(any(debug_assertions, feature = "oracle"))]
    RecordWrite(std::io::ErrorKind),
}

impl std::fmt::Display for ClockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeUnixEpoch => formatter.write_str("the clock is before the Unix epoch"),
            Self::OutOfRange => formatter.write_str("the clock exceeds the millisecond range"),
            #[cfg(any(debug_assertions, feature = "oracle"))]
            Self::RecordWrite(kind) => {
                write!(formatter, "the oracle clock record could not be written: {kind}")
            }
        }
    }
}

impl std::error::Error for ClockError {}

/// The fixture epoch every virtual clock starts at: 2026-01-01T00:00:00Z.
/// Snapshot times under the oracle are offsets from it.
#[cfg(any(debug_assertions, feature = "oracle"))]
pub(crate) const FIXTURE_EPOCH_MS: u64 = 1_767_225_600_000;

/// Time that moves only when the oracle advances it.
#[cfg(any(debug_assertions, feature = "oracle"))]
#[derive(Debug, Default)]
pub(crate) struct VirtualClock {
    /// Milliseconds advanced past [`FIXTURE_EPOCH_MS`].
    offset_ms: AtomicU64,
    /// The launch record a persistent owner keeps the offset in.
    record: Option<record::RecordFile>,
}

#[cfg(any(debug_assertions, feature = "oracle"))]
impl VirtualClock {
    pub(crate) fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(FIXTURE_EPOCH_MS + self.offset_ms())
    }

    pub(crate) fn offset_ms(&self) -> u64 {
        self.offset_ms.load(Ordering::Relaxed)
    }

    /// Moves the clock `ms` forward; the new offset. Refuses a move past the
    /// largest time a millisecond count since the Unix epoch holds. A
    /// persistent owner's record holds the offset before this returns.
    pub(crate) fn advance(&self, ms: u64) -> Result<u64, ClockError> {
        let limit = u64::MAX - FIXTURE_EPOCH_MS;
        let next = self
            .offset_ms
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |offset| {
                offset.checked_add(ms).filter(|next| *next <= limit)
            })
            .map(|previous| previous + ms)
            .map_err(|_| ClockError::OutOfRange)?;
        if let Some(record) = &self.record {
            // The offset only grows, so the writer whose read after its own
            // rename matches what it wrote leaves the latest offset on disk.
            let mut written = next;
            loop {
                record.write(written).map_err(|error| ClockError::RecordWrite(error.kind()))?;
                let current = self.offset_ms();
                if current == written {
                    break;
                }
                written = current;
            }
        }
        Ok(next)
    }
}

/// Where a store's random id bytes come from.
#[derive(Clone, Debug, Default)]
pub(crate) enum IdSource {
    /// The operating system's random source.
    #[default]
    System,
    #[cfg(any(debug_assertions, feature = "oracle"))]
    Seeded(Arc<SeededIds>),
}

/// The id source failed to give bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdError;

impl std::fmt::Display for IdError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("operating system randomness unavailable")
    }
}

impl std::error::Error for IdError {}

impl IdSource {
    /// Fills `bytes` with id randomness.
    pub(crate) fn fill(&self, bytes: &mut [u8]) -> Result<(), IdError> {
        match self {
            Self::System => getrandom::fill(bytes).map_err(|_| IdError),
            #[cfg(any(debug_assertions, feature = "oracle"))]
            Self::Seeded(ids) => {
                ids.fill(bytes);
                Ok(())
            }
        }
    }
}

/// A ChaCha20 keystream keyed by the oracle seed. Each [`Self::fill`] takes
/// whole 64-byte blocks from a shared block counter, so ids are a function of
/// the seed and the order of the mints, and no lock is held to draw one.
#[cfg(any(debug_assertions, feature = "oracle"))]
#[derive(Debug)]
pub(crate) struct SeededIds {
    key: [u32; 8],
    /// The ChaCha20 stream id: the launch on this state root (`record.rs`).
    stream: u64,
    next_block: AtomicU64,
}

#[cfg(any(debug_assertions, feature = "oracle"))]
impl SeededIds {
    /// Domain-separated so the key is never the seed's bare digest.
    const KEY_CONTEXT: &'static [u8] = b"cmux-tui oracle ids v1\0";

    pub(crate) fn new(seed: &str) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::new().chain_update(Self::KEY_CONTEXT).chain_update(seed).finalize();
        let mut key = [0_u32; 8];
        for (word, bytes) in key.iter_mut().zip(digest.chunks_exact(4)) {
            *word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        Self { key, stream: 0, next_block: AtomicU64::new(0) }
    }

    /// The same seed's stream `stream`, from its first block.
    fn stream(&self, stream: u64) -> Self {
        Self { key: self.key, stream, next_block: AtomicU64::new(0) }
    }

    fn fill(&self, bytes: &mut [u8]) {
        let blocks = u64::try_from(bytes.len().div_ceil(BLOCK_BYTES)).unwrap_or(u64::MAX);
        let first = self.next_block.fetch_add(blocks, Ordering::Relaxed);
        for (index, chunk) in (first..).zip(bytes.chunks_mut(BLOCK_BYTES)) {
            let low = index as u32;
            let high = (index >> 32) as u32;
            let stream = [self.stream as u32, (self.stream >> 32) as u32];
            let block = chacha20_block(&self.key, [low, high, stream[0], stream[1]]);
            chunk.copy_from_slice(&block[..chunk.len()]);
        }
    }
}

#[cfg(any(debug_assertions, feature = "oracle"))]
const BLOCK_BYTES: usize = 64;

/// The ChaCha20 block function (RFC 8439 section 2.3) of `key` and the last
/// four state words (`counter_nonce`: the 64-bit block counter, low word
/// first, then the 64-bit stream id).
#[cfg(any(debug_assertions, feature = "oracle"))]
fn chacha20_block(key: &[u32; 8], counter_nonce: [u32; 4]) -> [u8; BLOCK_BYTES] {
    const CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];
    let mut initial = [0_u32; 16];
    initial[..4].copy_from_slice(&CONSTANTS);
    initial[4..12].copy_from_slice(key);
    initial[12..].copy_from_slice(&counter_nonce);
    let mut state = initial;
    let quarter = |state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize| {
        state[a] = state[a].wrapping_add(state[b]);
        state[d] = (state[d] ^ state[a]).rotate_left(16);
        state[c] = state[c].wrapping_add(state[d]);
        state[b] = (state[b] ^ state[c]).rotate_left(12);
        state[a] = state[a].wrapping_add(state[b]);
        state[d] = (state[d] ^ state[a]).rotate_left(8);
        state[c] = state[c].wrapping_add(state[d]);
        state[b] = (state[b] ^ state[c]).rotate_left(7);
    };
    for _ in 0..10 {
        quarter(&mut state, 0, 4, 8, 12);
        quarter(&mut state, 1, 5, 9, 13);
        quarter(&mut state, 2, 6, 10, 14);
        quarter(&mut state, 3, 7, 11, 15);
        quarter(&mut state, 0, 5, 10, 15);
        quarter(&mut state, 1, 6, 11, 12);
        quarter(&mut state, 2, 7, 8, 13);
        quarter(&mut state, 3, 4, 9, 14);
    }
    let mut out = [0_u8; BLOCK_BYTES];
    for ((bytes, word), start) in out.chunks_exact_mut(4).zip(state).zip(initial) {
        bytes.copy_from_slice(&word.wrapping_add(start).to_le_bytes());
    }
    out
}

#[cfg(all(test, any(debug_assertions, feature = "oracle")))]
#[path = "oracle_env_tests.rs"]
mod tests;

#[cfg(all(test, any(debug_assertions, feature = "oracle")))]
#[path = "oracle_env_stores_tests.rs"]
mod stores_tests;

#[cfg(all(test, any(debug_assertions, feature = "oracle")))]
#[path = "oracle_env_restart_tests.rs"]
mod restart_tests;
