//! The broker thread: shards, worker lives, advances, answers and reseeds.
//!
//! One thread owns every piece of state (no lock): the daemon's calls and
//! the links' frames arrive on one queue and are handled in order.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cmux_threads::ThreadQos;

use super::answers::{AnswerRejected, QueryAnswer};
use super::launcher::{WorkerLauncher, WorkerProcess};
use super::link::{Link, LinkEnd, LinkEvent, Outbound, SendRefused};
use super::reseed::{Reseed, SeedPlan};
use super::strikes::{ReseedMode, StrikeLog};
use super::wire::{self, Frame, Kind};

/// A terminal as the broker names it (the VT pool's token).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VtTerminalKey(pub u64);

/// A terminal handed to the broker.
#[derive(Debug)]
pub struct VtTerminalSpec {
    pub key: VtTerminalKey,
    pub cols: u16,
    pub rows: u16,
    /// The terminal's ring segment: a dup the broker keeps, so every worker
    /// the terminal's shard gets can be handed the ring.
    pub ring: OwnedFd,
    /// Output before this offset was parsed, and its queries answered, by
    /// an earlier owner (the daemon before a restart: the highest offset it
    /// acknowledged, `cmux_holder::Listed::answered`; 0 for a new
    /// terminal). A worker parses it again from the ring and its answers
    /// are dropped.
    pub answered: u64,
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
    /// Hand the holder `replay`, a VT replay of the terminal's screen at
    /// output offset `base`, as its prologue (the holder's SNAPSHOT, as the
    /// in-daemon parser's snapshots): the worker parsed up to `base`.
    fn snapshot(&mut self, terminal: VtTerminalKey, base: u64, replay: Vec<u8>);
    /// The worker parsed every byte of the terminal before `offset` (the
    /// sink's `confirm_parsed`, the holder's ACK).
    fn parsed(&mut self, terminal: VtTerminalKey, offset: u64);
    /// A worker started, ended, was refused or restored its shard (debug.vt,
    /// the census, tests).
    fn lifecycle(&mut self, event: WorkerLifecycle);
}

/// What happened to a shard's worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerLifecycle {
    Started {
        shard: usize,
        generation: u64,
        pid: u32,
    },
    LaunchFailed {
        shard: usize,
        error: io::ErrorKind,
    },
    /// The worker's link ended; `next` is how its successor reseeds.
    Lost {
        shard: usize,
        generation: u64,
        cause: LinkEnd,
        strikes: usize,
        next: ReseedMode,
    },
    /// Every terminal of the shard reached the end its seed named, `elapsed`
    /// after the loss (or the start) that needed the seed.
    Restored {
        shard: usize,
        generation: u64,
        terminals: usize,
        mode: ReseedMode,
        elapsed: Duration,
    },
    /// The worker speaks another wire version; the shard gets no worker.
    Refused {
        shard: usize,
        generation: u64,
        version: u16,
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
    /// Frames a worker's queue holds before the broker calls it hung. At
    /// most one advance per terminal is in flight, so a worker that reads
    /// never comes near it.
    pub queue: usize,
}

impl BrokerConfig {
    /// Half the cores, at most 4 (the VT pool's shard count).
    pub fn for_machine() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, NonZeroUsize::get);
        Self { shards: (cores / 2).clamp(1, 4), queue: 4096 }
    }
}

/// The clock the strike rule and restore times read (a test's is fake).
pub type BrokerClock = Box<dyn FnMut() -> Instant + Send>;

pub(super) enum BrokerMsg {
    Attach(VtTerminalSpec),
    Advance(VtTerminalKey, u64),
    Resize(VtTerminalKey, u16, u16),
    Detach(VtTerminalKey),
    Link { shard: usize, generation: u64, event: LinkEvent },
    Shutdown,
}

/// The daemon's handle: every call queues and returns.
pub struct VtWorkerBroker {
    events: Sender<BrokerMsg>,
    thread: Option<JoinHandle<()>>,
}

impl VtWorkerBroker {
    /// Start the broker thread. No worker starts before a terminal attaches.
    pub fn start(
        config: BrokerConfig,
        launcher: Box<dyn WorkerLauncher>,
        host: Box<dyn VtWorkerHost>,
        clock: BrokerClock,
    ) -> io::Result<Self> {
        let (events, inbox) = channel();
        let state = Broker::new(config, launcher, host, clock, events.clone());
        let thread =
            cmux_threads::spawn("vt-broker", ThreadQos::UserInitiated, move || state.run(&inbox))?;
        Ok(Self { events, thread: Some(thread) })
    }

    /// Parse `spec`'s terminal in a worker from its holder prologue on.
    pub fn attach(&self, spec: VtTerminalSpec) {
        let _ = self.events.send(BrokerMsg::Attach(spec));
    }

    /// The terminal's ring holds output up to `end`.
    pub fn advance(&self, terminal: VtTerminalKey, end: u64) {
        let _ = self.events.send(BrokerMsg::Advance(terminal, end));
    }

    pub fn resize(&self, terminal: VtTerminalKey, cols: u16, rows: u16) {
        let _ = self.events.send(BrokerMsg::Resize(terminal, cols, rows));
    }

    pub fn detach(&self, terminal: VtTerminalKey) {
        let _ = self.events.send(BrokerMsg::Detach(terminal));
    }
}

impl Drop for VtWorkerBroker {
    /// Ends every worker (SIGKILL: a worker keeps no state the holder does
    /// not have) and the broker thread.
    fn drop(&mut self) {
        let _ = self.events.send(BrokerMsg::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Terminal {
    shard: usize,
    cols: u16,
    rows: u16,
    ring: Arc<OwnedFd>,
    /// The newest ring end the daemon reported.
    end: u64,
    /// The newest end sent to the worker (an advance or a seed's end).
    sent: u64,
    /// What the current worker reported parsed (from its seed on).
    parsed: u64,
    /// The newest offset handed to [`VtWorkerHost::parsed`]: it never goes
    /// back when a new worker reparses from the prologue.
    confirmed: u64,
    /// Output before here had its queries answered (by the daemon before a
    /// restart, or by a worker that reported it parsed): parsed again, it
    /// answers nothing.
    answered: u64,
    /// The checked answers of the parse in progress, written when its
    /// Parsed comes, in the same step that moves `answered` past it: a
    /// worker lost between the two leaves the query unanswered for its
    /// successor, never answered twice.
    held: Vec<QueryAnswer>,
    /// The base of the newest snapshot handed to the holder.
    snapshot_base: u64,
}

struct Worker {
    link: Link,
    process: Box<dyn WorkerProcess>,
}

struct Restore {
    started: Instant,
    mode: ReseedMode,
    /// Terminals whose seed's end the worker has not reported parsed.
    pending: BTreeMap<VtTerminalKey, u64>,
}

#[derive(Default)]
struct Shard {
    generation: u64,
    worker: Option<Worker>,
    strikes: StrikeLog,
    members: BTreeSet<VtTerminalKey>,
    restore: Option<Restore>,
    /// The worker's queue overflowed: replaced after the current message.
    hung: bool,
    /// A worker of another wire version: no more launches.
    refused: bool,
}

struct Broker {
    config: BrokerConfig,
    launcher: Box<dyn WorkerLauncher>,
    host: Box<dyn VtWorkerHost>,
    clock: BrokerClock,
    events: Sender<BrokerMsg>,
    shards: Vec<Shard>,
    terminals: BTreeMap<VtTerminalKey, Terminal>,
}

impl Broker {
    fn new(
        config: BrokerConfig,
        launcher: Box<dyn WorkerLauncher>,
        host: Box<dyn VtWorkerHost>,
        clock: BrokerClock,
        events: Sender<BrokerMsg>,
    ) -> Self {
        let shards = (0..config.shards.max(1)).map(|_| Shard::default()).collect();
        Self { config, launcher, host, clock, events, shards, terminals: BTreeMap::new() }
    }

    fn run(mut self, inbox: &Receiver<BrokerMsg>) {
        for msg in inbox {
            match msg {
                BrokerMsg::Attach(spec) => self.attach(spec),
                BrokerMsg::Advance(key, end) => self.advance(key, end),
                BrokerMsg::Resize(key, cols, rows) => self.resize(key, cols, rows),
                BrokerMsg::Detach(key) => self.detach(key),
                BrokerMsg::Link { shard, generation, event } => {
                    self.link_event(shard, generation, event);
                }
                BrokerMsg::Shutdown => break,
            }
            self.replace_hung();
        }
        for shard in &mut self.shards {
            if let Some(worker) = shard.worker.take() {
                retire(worker);
            }
        }
    }

    fn attach(&mut self, spec: VtTerminalSpec) {
        if self.terminals.contains_key(&spec.key) {
            return;
        }
        let shard = (0..self.shards.len())
            .min_by_key(|&index| self.shards[index].members.len())
            .unwrap_or(0);
        let terminal = Terminal {
            shard,
            cols: spec.cols,
            rows: spec.rows,
            ring: Arc::new(spec.ring),
            end: 0,
            sent: 0,
            parsed: 0,
            confirmed: 0,
            answered: spec.answered,
            held: Vec::new(),
            snapshot_base: 0,
        };
        self.terminals.insert(spec.key, terminal);
        self.shards[shard].members.insert(spec.key);
        let started = (self.clock)();
        if self.shards[shard].worker.is_none() {
            return self.launch(shard, ReseedMode::Full, started);
        }
        // A new terminal is seeded in full whatever the shard's strikes: its
        // stream never ran in a worker. It joins a restore under way, or
        // starts one of its own.
        self.shards[shard].restore.get_or_insert_with(|| Restore {
            started,
            mode: ReseedMode::Full,
            pending: BTreeMap::new(),
        });
        self.seed(spec.key, ReseedMode::Full);
    }

    fn advance(&mut self, key: VtTerminalKey, end: u64) {
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        terminal.end = terminal.end.max(end);
        self.send_advance(key);
    }

    /// Send the terminal's end when the worker reached the last one sent:
    /// at most one advance in flight per terminal. The worker reads the
    /// ring's published end itself, so an advance is a wake, not data.
    fn send_advance(&mut self, key: VtTerminalKey) {
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        if terminal.end <= terminal.sent || terminal.parsed < terminal.sent {
            return;
        }
        terminal.sent = terminal.end;
        let (shard, end) = (terminal.shard, terminal.end);
        self.send(shard, Frame::advance(key.0, end), None);
    }

    fn resize(&mut self, key: VtTerminalKey, cols: u16, rows: u16) {
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        (terminal.cols, terminal.rows) = (cols, rows);
        let shard = terminal.shard;
        self.send(shard, Frame::resize(key.0, cols, rows), None);
    }

    fn detach(&mut self, key: VtTerminalKey) {
        let Some(terminal) = self.terminals.remove(&key) else { return };
        let shard = terminal.shard;
        self.shards[shard].members.remove(&key);
        self.send(shard, Frame::release(key.0), None);
        self.settle_restore(shard, Some((key, u64::MAX)));
    }

    /// Start a worker for `shard` and seed every terminal of it in `mode`.
    /// `started`: when the need for it arose (the attach or the loss), the
    /// start of the restore it reports.
    fn launch(&mut self, shard: usize, mode: ReseedMode, started: Instant) {
        if self.shards[shard].refused {
            return;
        }
        let generation = self.shards[shard].generation + 1;
        let launched = self.launcher.launch(shard).and_then(|launched| {
            let mut process = launched.process;
            match Link::start(shard, generation, launched.link, self.config.queue, &self.events) {
                Ok(link) => Ok(Worker { link, process }),
                Err(error) => {
                    // No link, no use: it must not outlive the failure.
                    process.kill();
                    process.reap();
                    Err(error)
                }
            }
        });
        let worker = match launched {
            Ok(worker) => worker,
            Err(error) => {
                self.host.lifecycle(WorkerLifecycle::LaunchFailed { shard, error: error.kind() });
                return;
            }
        };
        let slot = &mut self.shards[shard];
        slot.generation += 1;
        let (generation, pid) = (slot.generation, worker.process.pid());
        slot.worker = Some(worker);
        slot.restore = Some(Restore { started, mode, pending: BTreeMap::new() });
        self.host.lifecycle(WorkerLifecycle::Started { shard, generation, pid });
        // Queued before the worker is ready, as Chromium queues calls on a
        // service's remote before its process runs.
        self.send(shard, Frame::hello(shard as u32), None);
        let members: Vec<_> = self.shards[shard].members.iter().copied().collect();
        for key in members {
            self.seed(key, mode);
        }
        self.settle_restore(shard, None);
    }

    /// Hand `key`'s ring to its shard's worker and seed it.
    fn seed(&mut self, key: VtTerminalKey, mode: ReseedMode) {
        let Some(reseed) = self.host.reseed(key) else {
            // Gone on the holder side: forget it.
            if let Some(terminal) = self.terminals.remove(&key) {
                self.shards[terminal.shard].members.remove(&key);
            }
            return;
        };
        let mut plan = SeedPlan::new(reseed, mode);
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        let end = plan.end;
        // The seed stops where the answered output ends and an advance
        // takes the worker on, so no parse spans both: an answer that
        // comes while the worker has not reached it is a replay's.
        if terminal.answered > plan.replay_from && terminal.answered < end {
            plan.end = terminal.answered;
        }
        terminal.end = terminal.end.max(end);
        terminal.sent = plan.end;
        terminal.parsed = plan.replay_from;
        // A dead worker's unconfirmed answers: the new one parses it again.
        terminal.held.clear();
        let (shard, cols, rows, ring) =
            (terminal.shard, terminal.cols, terminal.rows, terminal.ring.clone());
        if let Some(restore) = &mut self.shards[shard].restore {
            restore.pending.insert(key, end);
        }
        self.send(shard, Frame::assign(key.0, cols, rows), Some(ring));
        self.send(shard, Frame::seed(key.0, &plan.frame()), None);
    }

    /// `reached`: a terminal reached an offset; it leaves its shard's
    /// restore when that is its seed's end. The restore ends with its last
    /// terminal.
    fn settle_restore(&mut self, shard: usize, reached: Option<(VtTerminalKey, u64)>) {
        let slot = &mut self.shards[shard];
        let Some(restore) = &mut slot.restore else { return };
        if let Some((key, offset)) = reached
            && restore.pending.get(&key).is_some_and(|&end| offset >= end)
        {
            restore.pending.remove(&key);
        }
        if !restore.pending.is_empty() || slot.worker.is_none() {
            return;
        }
        let Some(restore) = slot.restore.take() else { return };
        let (generation, terminals) = (slot.generation, slot.members.len());
        let elapsed = (self.clock)().saturating_duration_since(restore.started);
        self.host.lifecycle(WorkerLifecycle::Restored {
            shard,
            generation,
            terminals,
            mode: restore.mode,
            elapsed,
        });
    }

    fn link_event(&mut self, shard: usize, generation: u64, event: LinkEvent) {
        let Some(slot) = self.shards.get(shard) else { return };
        if slot.generation != generation || slot.worker.is_none() {
            // A retired worker's last words.
            return;
        }
        match event {
            LinkEvent::Ended(cause) => self.lost(shard, cause),
            LinkEvent::Frame(frame) => self.frame(shard, generation, frame),
        }
    }

    fn frame(&mut self, shard: usize, generation: u64, frame: Frame) {
        let key = VtTerminalKey(frame.terminal);
        match frame.kind {
            Kind::Ready => match wire::decode_version(&frame) {
                Ok(wire::WIRE_VERSION) => {}
                Ok(version) => self.refuse(shard, generation, version),
                Err(error) => self.lost(shard, LinkEnd::BadFrame(error.to_string())),
            },
            Kind::Parsed => match wire::decode_offset(&frame) {
                Ok(offset) => self.parsed(shard, key, offset),
                Err(error) => self.lost(shard, LinkEnd::BadFrame(error.to_string())),
            },
            Kind::Answer => self.answer(shard, key, &frame.payload),
            Kind::Snapshot => match wire::decode_snapshot(&frame) {
                Ok((base, replay)) => self.snapshot(shard, key, base, replay),
                Err(error) => self.lost(shard, LinkEnd::BadFrame(error.to_string())),
            },
            // A daemon-to-worker kind from a worker.
            _ => self.lost(shard, LinkEnd::BadFrame(format!("{:?} from a worker", frame.kind))),
        }
    }

    fn parsed(&mut self, shard: usize, key: VtTerminalKey, offset: u64) {
        // A terminal released while its report was on the way, or another
        // shard's: nothing to confirm.
        let Some(terminal) =
            self.terminals.get_mut(&key).filter(|terminal| terminal.shard == shard)
        else {
            return;
        };
        // Never past what the ring holds: the holder trims by this offset.
        let offset = offset.min(terminal.end);
        terminal.parsed = terminal.parsed.max(offset);
        // The parse's answers go out with the move of `answered` past it:
        // a successor's replay of it answers nothing.
        terminal.answered = terminal.answered.max(terminal.parsed);
        for answer in std::mem::take(&mut terminal.held) {
            self.host.write_answer(key, &answer.encode());
        }
        if offset > terminal.confirmed {
            terminal.confirmed = offset;
            self.host.parsed(key, offset);
        }
        self.settle_restore(shard, Some((key, offset)));
        self.send_advance(key);
    }

    /// The shard owner's prologue snapshot (p01-process-210): relayed to
    /// the holder when the worker parsed up to its base and it is newer
    /// than the last one. A base the worker never reported parsed is a bad
    /// message: the holder would keep a screen of output that does not
    /// exist yet.
    fn snapshot(&mut self, shard: usize, key: VtTerminalKey, base: u64, replay: &[u8]) {
        let Some(terminal) =
            self.terminals.get_mut(&key).filter(|terminal| terminal.shard == shard)
        else {
            return;
        };
        if base > terminal.parsed {
            let cause = format!("snapshot at {base} past the parsed {}", terminal.parsed);
            return self.lost(shard, LinkEnd::BadFrame(cause));
        }
        if base <= terminal.snapshot_base {
            return;
        }
        terminal.snapshot_base = base;
        self.host.snapshot(key, base, replay.to_vec());
    }

    /// A worker's answer: checked now, held until the Parsed of its parse
    /// ([`Self::parsed`]).
    fn answer(&mut self, shard: usize, key: VtTerminalKey, bytes: &[u8]) {
        let checked = match self.terminals.get_mut(&key).filter(|terminal| terminal.shard == shard)
        {
            // Its parse started before the answered output ends (a seed
            // stops there): a replay, answered when it first ran.
            Some(terminal) if terminal.parsed < terminal.answered => Err(AnswerRejected::Replayed),
            Some(terminal) if terminal.held.len() >= MAX_HELD_ANSWERS => {
                let cause = format!("{MAX_HELD_ANSWERS} answers without a Parsed");
                return self.lost(shard, LinkEnd::BadFrame(cause));
            }
            Some(terminal) => QueryAnswer::parse(bytes, terminal.cols, terminal.rows)
                .map(|answer| terminal.held.push(answer)),
            None => Err(AnswerRejected::NotInShard),
        };
        if let Err(reason) = checked {
            self.host.lifecycle(WorkerLifecycle::AnswerRejected { shard, terminal: key, reason });
        }
    }

    /// Queue a frame for `shard`'s worker; a full queue marks it hung.
    fn send(&mut self, shard: usize, frame: Frame, fd: Option<Arc<OwnedFd>>) {
        let slot = &mut self.shards[shard];
        let Some(worker) = &slot.worker else { return };
        if let Err(SendRefused::Full) = worker.link.send(Outbound { frame, fd }) {
            slot.hung = true;
        }
        // `Ended`: the link's end is queued behind this message.
    }

    fn replace_hung(&mut self) {
        for shard in 0..self.shards.len() {
            if std::mem::take(&mut self.shards[shard].hung) && self.shards[shard].worker.is_some() {
                self.lost(shard, LinkEnd::Hung);
            }
        }
    }

    /// The worker of `shard` is gone or must go: strike, replace, reseed.
    fn lost(&mut self, shard: usize, cause: LinkEnd) {
        let Some(old) = self.shards[shard].worker.take() else { return };
        let now = (self.clock)();
        let slot = &mut self.shards[shard];
        let next = slot.strikes.strike(now);
        let (generation, strikes) = (slot.generation, slot.strikes.recent(now));
        slot.restore = None;
        let link = kill(old);
        // Its parses never confirmed: their answers are not written.
        for terminal in self.terminals.values_mut().filter(|terminal| terminal.shard == shard) {
            terminal.held.clear();
        }
        self.host.lifecycle(WorkerLifecycle::Lost { shard, generation, cause, strikes, next });
        if !self.shards[shard].members.is_empty() {
            self.launch(shard, next, now);
        }
        reap(link);
    }

    fn refuse(&mut self, shard: usize, generation: u64, version: u16) {
        let slot = &mut self.shards[shard];
        slot.refused = true;
        slot.restore = None;
        if let Some(worker) = slot.worker.take() {
            retire(worker);
        }
        self.host.lifecycle(WorkerLifecycle::Refused { shard, generation, version });
    }
}

/// Answers one parse may hold before its Parsed. A worker reports every
/// slice it parses (at most 64 KiB, cmux-vt-worker `SLICE_BYTES`; a seed's
/// prologue at most 128 KiB), and each answer needs a query of at least 3
/// bytes in it: past this the worker is lying.
const MAX_HELD_ANSWERS: usize = 64 * 1024;

/// SIGKILL the worker; its link and process are reaped by [`reap`] once
/// its successor is launched, so the restore does not wait for the reaping.
fn kill(mut worker: Worker) -> Worker {
    worker.process.kill();
    worker
}

fn reap(mut worker: Worker) {
    worker.process.reap();
    drop(worker.link);
}

fn retire(worker: Worker) {
    reap(kill(worker));
}
