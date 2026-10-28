//! The broker's seam to the daemon: the terminals it is handed, the host
//! it calls back, what it reports and how it is sized.

use std::io;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cmux_kitty_media::MediaRequest;
use cmux_vt_worker::sandbox::{self, SandboxError, SandboxMechanism, SandboxPolicy};
use cmux_vt_worker::wire::{AttachFailure, Colors, Event, Geometry};
use ghostty_vt::KittyGraphicsLimits;

use super::answers::AnswerRejected;
use super::broker::MediaReply;
use super::link::LinkEnd;
use super::reseed::Reseed;
use super::strikes::{ReseedMode, Restart};

/// A terminal as the broker names it (the VT pool's token).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VtTerminalKey(pub u64);

/// A terminal handed to the broker.
#[derive(Debug)]
pub struct VtTerminalSpec {
    pub key: VtTerminalKey,
    pub geometry: Geometry,
    /// The terminal's ring segment: a dup the broker keeps, so every worker
    /// the terminal's shard gets can be handed the ring.
    pub ring: OwnedFd,
    /// The ring's data bytes, as its header says.
    pub ring_capacity: u64,
    /// The parser's retained scrollback, in bytes.
    pub scrollback: u64,
    pub kitty_limits: KittyGraphicsLimits,
    /// Default colors, palette and cursor, when the daemon sets any.
    pub colors: Option<Colors>,
    /// Output before this offset was parsed, and its queries answered,
    /// before the broker took the terminal: a worker that reparses it
    /// writes no answer for it again.
    pub answered: u64,
}

/// A terminal's screen page: its worker publishes into it, the daemon
/// reads it (`cmux_vt_worker::screen_page::PageReader`).
#[derive(Clone, Debug)]
pub struct ScreenPage {
    pub fd: Arc<OwnedFd>,
    pub len: u64,
}

/// The daemon side of the broker. Every call runs on the broker thread and
/// must not wait: it reads cached state or queues work.
pub trait VtWorkerHost: Send + 'static {
    /// The terminal's newest prologue and its ring's span, for a (re)seed;
    /// `None` when the terminal is gone (the broker forgets it).
    fn reseed(&mut self, terminal: VtTerminalKey) -> Option<Reseed>;
    /// Write a validated query answer (canonical bytes) to the terminal's
    /// PTY input.
    fn write_answer(&mut self, terminal: VtTerminalKey, answer: &[u8]);
    /// The worker parsed every byte of the terminal before `offset` (the
    /// sink's `confirm_parsed`, the holder's ACK).
    fn parsed(&mut self, terminal: VtTerminalKey, offset: u64);
    /// The terminal's screen is published in `page` from now on.
    fn screen_page(&mut self, terminal: VtTerminalKey, page: &ScreenPage);
    /// A title, cwd, bell, OSC 7501 status or ring lap of the terminal's
    /// output, as the worker parsed it.
    fn terminal_event(&mut self, terminal: VtTerminalKey, event: Event);
    /// The worker needs a Kitty file read (it may open no file); the
    /// terminal's parse waits until `reply` is sent. Read off this thread.
    fn kitty_media(&mut self, terminal: VtTerminalKey, request: MediaRequest, reply: MediaReply);
    /// A worker started, ended, restored its shard or a terminal was handed
    /// back (debug.vt, the census, tests).
    fn lifecycle(&mut self, event: WorkerLifecycle);
}

/// Why terminals went back to the daemon, which then parses them another
/// way (VT pool threads) or not at all: a terminal handed back for
/// crashing kills the parser, and the daemon must not run that parser
/// in-process on it (Rule of 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandBack {
    /// The shard's worker speaks another wire version (an update swapped
    /// the binary).
    WireVersion(u16),
    /// The shard's worker runs under another sandbox than the platform's.
    Sandbox(SandboxMechanism),
    /// The shard's workers died or failed to launch too often.
    ShardCrashing,
    /// The terminal was being parsed at too many of its workers' deaths.
    TerminalCrashing,
    /// Every shard was handed back: no worker can take a terminal.
    NoShard,
    /// The worker could not attach the terminal.
    AttachFailed(AttachFailure),
    /// A page or a descriptor for the terminal could not be made.
    Resources(io::ErrorKind),
}

/// What happened to a shard's worker or terminals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerLifecycle {
    Started {
        shard: usize,
        generation: u64,
        pid: u32,
    },
    LaunchFailed {
        shard: usize,
        generation: u64,
        error: io::ErrorKind,
        deaths: usize,
        restart: Restart,
    },
    /// The worker applied its sandbox and speaks this wire: it gets rings.
    Ready {
        shard: usize,
        generation: u64,
        sandbox: SandboxMechanism,
    },
    /// The worker's link ended. `degraded`: the terminals in flight at the
    /// death that reseed with less than their full history.
    Lost {
        shard: usize,
        generation: u64,
        cause: LinkEnd,
        deaths: usize,
        degraded: Vec<(VtTerminalKey, ReseedMode)>,
        restart: Restart,
    },
    /// Every terminal of the shard reached the end its seed named, `elapsed`
    /// after the loss (or the start) that needed the seed; `mode` is the
    /// most degraded seed of the restore.
    Restored {
        shard: usize,
        generation: u64,
        terminals: usize,
        mode: ReseedMode,
        elapsed: Duration,
    },
    /// The broker no longer parses these terminals (`shard` is `None` for a
    /// terminal no shard could take).
    HandedBack {
        shard: Option<usize>,
        reason: HandBack,
        terminals: Vec<VtTerminalKey>,
    },
    /// An answer was not written.
    AnswerRejected {
        shard: usize,
        terminal: VtTerminalKey,
        reason: AnswerRejected,
    },
}

/// The broker's sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrokerConfig {
    /// Worker processes at most (shards).
    pub shards: usize,
    /// Commands a worker's queue holds before the broker calls it hung. At
    /// most one advance per terminal is in flight, so a worker that reads
    /// never comes near it.
    pub queue: usize,
    /// How long a new worker has to send HELLO.
    pub hello_timeout: Duration,
}

impl BrokerConfig {
    /// Half the cores, at most 4 (the VT pool's shard count).
    pub fn for_machine() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, NonZeroUsize::get);
        Self { shards: (cores / 2).clamp(1, 4), queue: 4096, hello_timeout: Duration::from_secs(5) }
    }
}

/// The sandbox every worker gets, compiled once by the daemon (Chrome's
/// `SeatbeltExecClient`), and the one its HELLO must report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerSandbox {
    policy: SandboxPolicy,
    required: SandboxMechanism,
}

impl WorkerSandbox {
    /// This platform's, the only one outside the broker's unit tests:
    /// Seatbelt on macOS; seccomp-bpf on Linux, with landlock when the
    /// kernel has it. A worker that reports anything else gets no ring.
    /// `Unsupported` where workers have no sandbox: none runs there.
    pub fn for_platform() -> Result<Self, SandboxError> {
        let policy = sandbox::compile_policy()?;
        let required = policy.mechanism();
        Ok(Self { policy, required })
    }

    /// The policy and requirement the unit tests' stand-in threads use (a
    /// thread cannot apply a sandbox).
    #[cfg(test)]
    pub(super) fn unchecked(policy: SandboxPolicy, required: SandboxMechanism) -> Self {
        Self { policy, required }
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    pub fn required(&self) -> SandboxMechanism {
        self.required
    }
}

/// The clock strikes, backoffs, deadlines and restore times read.
pub type BrokerClock = Box<dyn FnMut() -> Instant + Send>;
