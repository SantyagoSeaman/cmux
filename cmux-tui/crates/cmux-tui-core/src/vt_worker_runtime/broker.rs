//! The broker thread: shards, worker lives, advances, answers and reseeds.
//!
//! One thread owns every piece of state (no lock): the daemon's calls, the
//! links' events and the host's media answers arrive on one queue and are
//! handled in order. Relaunch backoffs and HELLO deadlines are the queue
//! wait's deadline (the injected clock decides when one is due).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use cmux_threads::ThreadQos;
use cmux_vt_worker::sandbox::SandboxMechanism;
use cmux_vt_worker::screen_page::{MAX_PAGE_BYTES, create_page, page_len_for};
use cmux_vt_worker::wire::{
    Attach, AttachFailure, Colors, Command, Event, Geometry, MAX_PROLOGUE, MediaAnswer,
    WIRE_VERSION,
};
use ghostty_vt::KittyGraphicsLimits;

use super::answers::{AnswerRejected, QueryAnswer};
use super::host::{
    BrokerClock, BrokerConfig, HandBack, ScreenPage, VtTerminalKey, VtTerminalSpec, VtWorkerHost,
    WorkerLifecycle, WorkerSandbox,
};
use super::launcher::{WorkerLauncher, WorkerProcess};
use super::link::{Link, LinkEnd, LinkEvent, SendRefused};
use super::reseed::SeedPlan;
use super::strikes::{ReseedMode, Restart, StrikeLog};

/// The answer to one [`VtWorkerHost::kitty_media`] request, from any
/// thread. An answer for a worker that died meanwhile is dropped: its
/// successor reparses the command and asks again.
pub struct MediaReply {
    events: Sender<BrokerMsg>,
    shard: usize,
    generation: u64,
    terminal: VtTerminalKey,
}

impl MediaReply {
    pub fn send(self, answer: MediaAnswer) {
        let Self { events, shard, generation, terminal } = self;
        let _ = events.send(BrokerMsg::KittyMedia { shard, generation, terminal, answer });
    }
}

/// Pending answer bytes of one terminal past which its worker is a bad
/// message: a parse slice of queries answers with far less.
const MAX_PENDING_ANSWER_BYTES: usize = 1 << 20;

pub(super) enum BrokerMsg {
    Attach(Box<VtTerminalSpec>),
    Advance(VtTerminalKey, u64),
    Resize(VtTerminalKey, Geometry),
    Colors(VtTerminalKey, Box<Colors>),
    KittyLimits(VtTerminalKey, KittyGraphicsLimits),
    Detach(VtTerminalKey),
    KittyMedia { shard: usize, generation: u64, terminal: VtTerminalKey, answer: MediaAnswer },
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
        sandbox: WorkerSandbox,
        launcher: Box<dyn WorkerLauncher>,
        host: Box<dyn VtWorkerHost>,
        clock: BrokerClock,
    ) -> io::Result<Self> {
        let (events, inbox) = channel();
        let state = Broker::new(config, sandbox, launcher, host, clock, events.clone());
        let thread =
            cmux_threads::spawn("vt-broker", ThreadQos::UserInitiated, move || state.run(&inbox))?;
        Ok(Self { events, thread: Some(thread) })
    }

    /// Parse `spec`'s terminal in a worker from its holder prologue on.
    pub fn attach(&self, spec: VtTerminalSpec) {
        let _ = self.events.send(BrokerMsg::Attach(Box::new(spec)));
    }

    /// The terminal's ring holds output up to `end`.
    pub fn advance(&self, terminal: VtTerminalKey, end: u64) {
        let _ = self.events.send(BrokerMsg::Advance(terminal, end));
    }

    pub fn resize(&self, terminal: VtTerminalKey, geometry: Geometry) {
        let _ = self.events.send(BrokerMsg::Resize(terminal, geometry));
    }

    pub fn colors(&self, terminal: VtTerminalKey, colors: Colors) {
        let _ = self.events.send(BrokerMsg::Colors(terminal, Box::new(colors)));
    }

    pub fn kitty_limits(&self, terminal: VtTerminalKey, limits: KittyGraphicsLimits) {
        let _ = self.events.send(BrokerMsg::KittyLimits(terminal, limits));
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
    geometry: Geometry,
    ring: Arc<OwnedFd>,
    ring_capacity: u64,
    scrollback: u64,
    kitty_limits: KittyGraphicsLimits,
    colors: Option<Colors>,
    page: ScreenPage,
    /// The newest ring end the daemon reported.
    end: u64,
    /// The newest end sent to the worker (its ATTACH start or an advance).
    sent: u64,
    /// What the current worker reported parsed (from its seed on).
    parsed: u64,
    /// The newest offset handed to [`VtWorkerHost::parsed`]: it never goes
    /// back when a new worker reparses from the prologue.
    confirmed: u64,
    /// The current worker has the terminal's ATTACH.
    attached: bool,
    /// The current worker reported a parse since the ATTACH (its prologue
    /// is parsed).
    first_parsed: bool,
    /// Answers whose output ends at or before this offset were written by
    /// an earlier worker (or are a prologue's): they are not written again.
    answer_floor: u64,
    /// Validated answers waiting for the PARSED that says which output
    /// made them (a worker sends a slice's answers before its PARSED).
    answers: Vec<Vec<u8>>,
    answer_bytes: usize,
    /// Worker deaths while this terminal's parse was in flight.
    strikes: StrikeLog,
    /// How the next worker seeds it.
    next_seed: ReseedMode,
}

impl Terminal {
    /// Whether a worker death now may have been this terminal's output:
    /// its seed or an advance was not reported parsed.
    fn in_flight(&self) -> bool {
        self.attached && (!self.first_parsed || self.parsed < self.sent)
    }
}

struct Worker {
    link: Link,
    process: Box<dyn WorkerProcess>,
    /// HELLO arrived and passed: commands other than BOOTSTRAP may go.
    ready: bool,
    hello_by: Instant,
}

struct Restore {
    started: Instant,
    /// The most degraded seed of the restore.
    mode: ReseedMode,
    /// Terminals whose seed's end the worker has not reported parsed.
    pending: BTreeMap<VtTerminalKey, u64>,
}

/// A launch waiting for its backoff.
struct Relaunch {
    at: Instant,
    /// When the need for it arose (the restore it reports starts there).
    started: Instant,
}

#[derive(Default)]
struct Shard {
    /// One per launch attempt, so the last words of a link that failed to
    /// start are never taken for a later worker's.
    generation: u64,
    worker: Option<Worker>,
    deaths: StrikeLog,
    members: BTreeSet<VtTerminalKey>,
    restore: Option<Restore>,
    relaunch: Option<Relaunch>,
    /// The worker's queue overflowed: replaced after the current message.
    hung: bool,
    /// Handed back for good: no more workers.
    failed: Option<HandBack>,
}

struct Broker {
    config: BrokerConfig,
    sandbox: WorkerSandbox,
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
        sandbox: WorkerSandbox,
        launcher: Box<dyn WorkerLauncher>,
        host: Box<dyn VtWorkerHost>,
        clock: BrokerClock,
        events: Sender<BrokerMsg>,
    ) -> Self {
        let shards = (0..config.shards.max(1)).map(|_| Shard::default()).collect();
        Self { config, sandbox, launcher, host, clock, events, shards, terminals: BTreeMap::new() }
    }

    fn run(mut self, inbox: &Receiver<BrokerMsg>) {
        loop {
            let msg = match self.next_deadline() {
                None => inbox.recv().ok(),
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since((self.clock)());
                    // wakeup-allow: deadline a pending relaunch backoff or HELLO deadline only; idle shards wait in recv
                    match inbox.recv_timeout(wait) {
                        Ok(msg) => Some(msg),
                        Err(RecvTimeoutError::Timeout) => {
                            self.due();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => None,
                    }
                }
            };
            match msg {
                None | Some(BrokerMsg::Shutdown) => break,
                Some(msg) => self.handle(msg),
            }
            self.replace_hung();
            self.due();
        }
        for shard in &mut self.shards {
            if let Some(worker) = shard.worker.take() {
                retire(worker);
            }
        }
    }

    fn handle(&mut self, msg: BrokerMsg) {
        match msg {
            BrokerMsg::Attach(spec) => self.attach(*spec),
            BrokerMsg::Advance(key, end) => self.advance(key, end),
            BrokerMsg::Resize(key, geometry) => self.resize(key, geometry),
            BrokerMsg::Colors(key, colors) => self.set_colors(key, *colors),
            BrokerMsg::KittyLimits(key, limits) => self.set_kitty_limits(key, limits),
            BrokerMsg::Detach(key) => self.detach(key),
            BrokerMsg::KittyMedia { shard, generation, terminal, answer } => {
                self.media_answer(shard, generation, terminal, answer);
            }
            BrokerMsg::Link { shard, generation, event } => {
                self.link_event(shard, generation, event);
            }
            BrokerMsg::Shutdown => {}
        }
    }

    /// The earliest relaunch or HELLO deadline.
    fn next_deadline(&self) -> Option<Instant> {
        self.shards
            .iter()
            .flat_map(|shard| {
                let hello = shard.worker.as_ref().filter(|w| !w.ready).map(|w| w.hello_by);
                [shard.relaunch.as_ref().map(|relaunch| relaunch.at), hello]
            })
            .flatten()
            .min()
    }

    /// Launch every shard whose backoff ended; end every worker past its
    /// HELLO deadline.
    fn due(&mut self) {
        let now = (self.clock)();
        for shard in 0..self.shards.len() {
            let slot = &mut self.shards[shard];
            if slot.worker.as_ref().is_some_and(|w| !w.ready && w.hello_by <= now) {
                self.lost(shard, LinkEnd::NoHello);
            }
            let slot = &mut self.shards[shard];
            if let Some(relaunch) = slot.relaunch.take_if(|relaunch| relaunch.at <= now) {
                self.launch(shard, relaunch.started);
            }
        }
    }

    fn attach(&mut self, spec: VtTerminalSpec) {
        let key = spec.key;
        if self.terminals.contains_key(&key) {
            return;
        }
        let Some(shard) = (0..self.shards.len())
            .filter(|&index| self.shards[index].failed.is_none())
            .min_by_key(|&index| self.shards[index].members.len())
        else {
            return self.host.lifecycle(WorkerLifecycle::HandedBack {
                shard: None,
                reason: HandBack::NoShard,
                terminals: vec![key],
            });
        };
        let page = match new_page(page_len_for(spec.geometry.cols, spec.geometry.rows)) {
            Ok(page) => page,
            Err(error) => {
                return self.host.lifecycle(WorkerLifecycle::HandedBack {
                    shard: Some(shard),
                    reason: HandBack::Resources(error.kind()),
                    terminals: vec![key],
                });
            }
        };
        self.host.screen_page(key, &page);
        let terminal = Terminal {
            shard,
            geometry: spec.geometry,
            ring: Arc::new(spec.ring),
            ring_capacity: spec.ring_capacity,
            scrollback: spec.scrollback,
            kitty_limits: spec.kitty_limits,
            colors: spec.colors,
            page,
            end: spec.answered,
            sent: 0,
            parsed: 0,
            confirmed: spec.answered,
            attached: false,
            first_parsed: false,
            answer_floor: spec.answered,
            answers: Vec::new(),
            answer_bytes: 0,
            strikes: StrikeLog::default(),
            next_seed: ReseedMode::Full,
        };
        self.terminals.insert(key, terminal);
        let slot = &mut self.shards[shard];
        slot.members.insert(key);
        let started = (self.clock)();
        let slot = &mut self.shards[shard];
        match &slot.worker {
            // Seeded with the rest of the shard once its worker says HELLO.
            None if slot.relaunch.is_none() => self.launch(shard, started),
            None => {}
            Some(worker) if !worker.ready => {}
            // A new terminal is seeded in full: its stream never ran in a
            // worker. It joins a restore under way, or starts one.
            Some(_) => {
                slot.restore.get_or_insert_with(|| Restore {
                    started,
                    mode: ReseedMode::Full,
                    pending: BTreeMap::new(),
                });
                self.seed(key);
            }
        }
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
        if !terminal.attached || terminal.end <= terminal.sent || terminal.parsed < terminal.sent {
            return;
        }
        terminal.sent = terminal.end;
        let (shard, end) = (terminal.shard, terminal.end);
        self.send(shard, Command::Advance { id: key.0, end });
    }

    fn resize(&mut self, key: VtTerminalKey, geometry: Geometry) {
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        terminal.geometry = geometry;
        let (shard, attached) = (terminal.shard, terminal.attached);
        let needed = page_len_for(geometry.cols, geometry.rows);
        let page = match self.grow_page(key, needed) {
            Ok(page) => page,
            Err(()) => return,
        };
        if attached {
            self.send(shard, Command::Resize { id: key.0, geometry, page });
        }
    }

    /// A worker's page is too small for its screen: give it a bigger one.
    fn page_too_small(&mut self, key: VtTerminalKey, needed: u64) {
        let Some(terminal) = self.terminals.get(&key) else { return };
        let (shard, geometry) = (terminal.shard, terminal.geometry);
        let Ok(needed) = usize::try_from(needed) else { return };
        if let Ok(Some(page)) = self.grow_page(key, needed) {
            self.send(shard, Command::Resize { id: key.0, geometry, page: Some(page) });
        }
    }

    /// A new page of at least `needed` bytes when the terminal's is
    /// smaller, as a descriptor for its worker. `Err`: the terminal was
    /// handed back (no page or descriptor could be made).
    fn grow_page(
        &mut self,
        key: VtTerminalKey,
        needed: usize,
    ) -> Result<Option<(OwnedFd, u64)>, ()> {
        let Some(terminal) = self.terminals.get_mut(&key) else { return Err(()) };
        let shard = terminal.shard;
        // A screen past the largest page stays on its old page: the worker
        // reports it too small and the daemon reads the last that fit.
        if needed > MAX_PAGE_BYTES || needed as u64 <= terminal.page.len {
            return Ok(None);
        }
        match new_page(needed).and_then(|page| Ok((page.fd.try_clone()?, page))) {
            Ok((fd, page)) => {
                let len = page.len;
                terminal.page = page;
                self.host.screen_page(key, &terminal.page);
                Ok(Some((fd, len)))
            }
            Err(error) => {
                self.hand_back(shard, vec![key], HandBack::Resources(error.kind()));
                Err(())
            }
        }
    }

    fn set_colors(&mut self, key: VtTerminalKey, colors: Colors) {
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        terminal.colors = Some(colors.clone());
        if terminal.attached {
            let shard = terminal.shard;
            self.send(shard, Command::Colors { id: key.0, colors });
        }
    }

    fn set_kitty_limits(&mut self, key: VtTerminalKey, limits: KittyGraphicsLimits) {
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        terminal.kitty_limits = limits;
        if terminal.attached {
            let shard = terminal.shard;
            self.send(shard, Command::KittyLimits { id: key.0, limits });
        }
    }

    fn detach(&mut self, key: VtTerminalKey) {
        let Some(terminal) = self.terminals.remove(&key) else { return };
        let shard = terminal.shard;
        self.shards[shard].members.remove(&key);
        if terminal.attached {
            self.send(shard, Command::Detach { id: key.0 });
        }
        self.settle_restore(shard, Some((key, u64::MAX)));
    }

    /// Start a worker for `shard` and send it its sandbox; its terminals
    /// are seeded when its HELLO arrives. `started`: when the need for it
    /// arose (the attach or the loss), the start of the restore it reports.
    fn launch(&mut self, shard: usize, started: Instant) {
        let slot = &mut self.shards[shard];
        if slot.failed.is_some() || slot.worker.is_some() {
            return;
        }
        slot.relaunch = None;
        slot.generation += 1;
        let generation = slot.generation;
        let launched = self.launcher.launch(shard).and_then(|launched| {
            let mut process = launched.process;
            match Link::start(shard, generation, launched.link, self.config.queue, &self.events) {
                Ok(link) => Ok((link, process)),
                Err(error) => {
                    // No link, no use: it must not outlive the failure.
                    process.kill();
                    process.reap();
                    Err(error)
                }
            }
        });
        let now = (self.clock)();
        let (link, process) = match launched {
            Ok(worker) => worker,
            Err(error) => {
                let deaths = self.shards[shard].deaths.strike(now);
                let restart = Restart::after(deaths);
                self.host.lifecycle(WorkerLifecycle::LaunchFailed {
                    shard,
                    generation,
                    error: error.kind(),
                    deaths,
                    restart,
                });
                return self.schedule(shard, restart, started, now);
            }
        };
        let pid = process.pid();
        let hello_by = now + self.config.hello_timeout;
        let slot = &mut self.shards[shard];
        slot.worker = Some(Worker { link, process, ready: false, hello_by });
        slot.restore = Some(Restore { started, mode: ReseedMode::Full, pending: BTreeMap::new() });
        self.host.lifecycle(WorkerLifecycle::Started { shard, generation, pid });
        // Chrome's SeatbeltExecClient: the policy is the first thing the
        // worker reads, and it gets no ring before it says it applied it.
        self.send(shard, Command::Bootstrap(self.sandbox.policy().clone()));
    }

    /// Relaunch `shard` per `restart`, or hand its terminals back.
    fn schedule(&mut self, shard: usize, restart: Restart, started: Instant, now: Instant) {
        match restart {
            Restart::GiveUp => self.hand_back_shard(shard, HandBack::ShardCrashing),
            // A shard with no terminal gets its worker with its next attach.
            Restart::After(_) if self.shards[shard].members.is_empty() => {}
            Restart::After(delay) if delay.is_zero() => self.launch(shard, started),
            Restart::After(delay) => {
                self.shards[shard].relaunch = Some(Relaunch { at: now + delay, started });
            }
        }
    }

    fn hello(&mut self, shard: usize, generation: u64, event: (u16, SandboxMechanism, u32)) {
        let (version, mechanism, pid) = event;
        let Some(worker) = &self.shards[shard].worker else { return };
        if worker.ready {
            return self.lost(shard, LinkEnd::BadFrame("a second HELLO".to_owned()));
        }
        if version != WIRE_VERSION {
            return self.hand_back_shard(shard, HandBack::WireVersion(version));
        }
        if mechanism != self.sandbox.required() {
            return self.hand_back_shard(shard, HandBack::Sandbox(mechanism));
        }
        let launched = worker.process.pid();
        if launched != 0 && pid != launched {
            let cause = format!("HELLO from pid {pid}, launched pid {launched}");
            return self.lost(shard, LinkEnd::BadFrame(cause));
        }
        if let Some(worker) = &mut self.shards[shard].worker {
            worker.ready = true;
        }
        self.host.lifecycle(WorkerLifecycle::Ready { shard, generation, sandbox: mechanism });
        // Terminals with no recent strike first: when a suspect's output
        // kills this worker too, its shard mates were parsed already and are
        // not taken for suspects.
        let now = (self.clock)();
        let mut members: Vec<_> = self.shards[shard]
            .members
            .iter()
            .map(|key| (self.terminals.get(key).map_or(0, |t| t.strikes.recent(now)), *key))
            .collect();
        members.sort_unstable();
        for (_, key) in members {
            self.seed(key);
        }
        self.settle_restore(shard, None);
    }

    /// Hand `key`'s ring and page to its shard's ready worker, with the
    /// screen to start from.
    fn seed(&mut self, key: VtTerminalKey) {
        let Some(reseed) = self.host.reseed(key) else {
            // Gone on the holder side: forget it.
            if let Some(terminal) = self.terminals.remove(&key) {
                self.shards[terminal.shard].members.remove(&key);
            }
            return;
        };
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        let mode = std::mem::replace(&mut terminal.next_seed, ReseedMode::Full);
        let SeedPlan { mut prologue, replay_from, end, mut skipped, .. } =
            SeedPlan::new(reseed, mode);
        if prologue.len() > MAX_PROLOGUE {
            // Larger than any the holder keeps (task 210 caps it at 128
            // KiB): the screen starts blank at `replay_from`.
            prologue = Vec::new();
            skipped = true;
        }
        let descriptors = terminal.ring.try_clone().and_then(|ring| {
            let page = terminal.page.fd.try_clone()?;
            Ok((ring, page))
        });
        let shard = terminal.shard;
        let (ring, page) = match descriptors {
            Ok(fds) => fds,
            Err(error) => {
                return self.hand_back(shard, vec![key], HandBack::Resources(error.kind()));
            }
        };
        terminal.end = terminal.end.max(end);
        terminal.parsed = replay_from;
        terminal.sent = replay_from;
        terminal.attached = true;
        terminal.first_parsed = false;
        terminal.answers.clear();
        terminal.answer_bytes = 0;
        // Answers to output an earlier worker parsed were written then. The
        // first advance stops at that offset, so no parse slice holds both
        // answered and unanswered output.
        terminal.answer_floor = terminal.confirmed.max(replay_from);
        let first_stop = terminal.confirmed.clamp(replay_from, end);
        let attach = Attach {
            id: key.0,
            start: replay_from,
            prologue,
            gap: skipped,
            ring,
            ring_capacity: terminal.ring_capacity,
            page,
            page_len: terminal.page.len,
            scrollback: terminal.scrollback,
            geometry: terminal.geometry,
            kitty_limits: terminal.kitty_limits,
        };
        let colors = terminal.colors.clone();
        if first_stop > replay_from {
            terminal.sent = first_stop;
        }
        if let Some(restore) = &mut self.shards[shard].restore {
            restore.pending.insert(key, end);
            restore.mode = restore.mode.max(mode);
        }
        self.send(shard, Command::Attach(attach));
        if let Some(colors) = colors {
            self.send(shard, Command::Colors { id: key.0, colors });
        }
        if first_stop > replay_from {
            self.send(shard, Command::Advance { id: key.0, end: first_stop });
        } else {
            self.send_advance(key);
        }
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
        if !restore.pending.is_empty() || !slot.worker.as_ref().is_some_and(|w| w.ready) {
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
            LinkEvent::Event(event) => self.event(shard, generation, event),
        }
    }

    fn event(&mut self, shard: usize, generation: u64, event: Event) {
        let ready = self.shards[shard].worker.as_ref().is_some_and(|worker| worker.ready);
        match event {
            Event::Hello { version, sandbox, pid } => {
                self.hello(shard, generation, (version, sandbox, pid));
            }
            event if !ready => {
                let cause = format!("{} before HELLO", event_name(&event));
                self.lost(shard, LinkEnd::BadFrame(cause));
            }
            Event::Parsed { id, offset, .. } => self.parsed(shard, VtTerminalKey(id), offset),
            Event::PtyWrite { id, bytes } => self.answer(shard, VtTerminalKey(id), &bytes),
            Event::PageTooSmall { id, needed } => {
                if self.attached_in(shard, VtTerminalKey(id)) {
                    self.page_too_small(VtTerminalKey(id), needed);
                }
            }
            Event::AttachFailed { id, reason } => {
                self.attach_failed(shard, VtTerminalKey(id), reason);
            }
            Event::KittyMediaRequest { id, request } => {
                let terminal = VtTerminalKey(id);
                if self.attached_in(shard, terminal) {
                    let events = self.events.clone();
                    let reply = MediaReply { events, shard, generation, terminal };
                    self.host.kitty_media(terminal, request, reply);
                }
            }
            event @ (Event::Title { id, .. }
            | Event::Pwd { id, .. }
            | Event::Bell { id, .. }
            | Event::Lapped { id, .. }
            | Event::ProgramStatus { id, .. }) => {
                if self.attached_in(shard, VtTerminalKey(id)) {
                    self.host.terminal_event(VtTerminalKey(id), event);
                }
            }
        }
    }

    /// The terminal is `shard`'s and its worker has its ATTACH. Events for
    /// others are a released terminal's last words (or another shard's,
    /// which task 141 makes a bad message).
    fn attached_in(&self, shard: usize, key: VtTerminalKey) -> bool {
        self.terminals
            .get(&key)
            .is_some_and(|terminal| terminal.shard == shard && terminal.attached)
    }

    fn parsed(&mut self, shard: usize, key: VtTerminalKey, offset: u64) {
        if !self.attached_in(shard, key) {
            return;
        }
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        // Never past what the ring holds: the holder trims by this offset.
        let offset = offset.min(terminal.end);
        terminal.first_parsed = true;
        terminal.parsed = terminal.parsed.max(offset);
        terminal.answer_bytes = 0;
        let answers = std::mem::take(&mut terminal.answers);
        // The answers came from the output this PARSED covers: written only
        // when it is output no earlier worker answered.
        if offset > terminal.answer_floor {
            for answer in &answers {
                self.host.write_answer(key, answer);
            }
        }
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        if offset > terminal.confirmed {
            terminal.confirmed = offset;
            self.host.parsed(key, offset);
        }
        self.settle_restore(shard, Some((key, offset)));
        self.send_advance(key);
    }

    fn answer(&mut self, shard: usize, key: VtTerminalKey, bytes: &[u8]) {
        let Some(terminal) = self.terminals.get(&key).filter(|terminal| terminal.shard == shard)
        else {
            let reason = AnswerRejected::NotInShard;
            return self.host.lifecycle(WorkerLifecycle::AnswerRejected {
                shard,
                terminal: key,
                reason,
            });
        };
        if !terminal.attached {
            return;
        }
        let (cols, rows) = (terminal.geometry.cols, terminal.geometry.rows);
        let mut accepted = Vec::new();
        for checked in QueryAnswer::parse_all(bytes, cols, rows) {
            match checked {
                Ok(answer) => accepted.push(answer.encode()),
                Err(reason) => self.host.lifecycle(WorkerLifecycle::AnswerRejected {
                    shard,
                    terminal: key,
                    reason,
                }),
            }
        }
        let Some(terminal) = self.terminals.get_mut(&key) else { return };
        terminal.answer_bytes += accepted.iter().map(Vec::len).sum::<usize>();
        terminal.answers.extend(accepted);
        if terminal.answer_bytes > MAX_PENDING_ANSWER_BYTES {
            self.lost(shard, LinkEnd::BadFrame("answers without a PARSED".to_owned()));
        }
    }

    fn attach_failed(&mut self, shard: usize, key: VtTerminalKey, reason: AttachFailure) {
        if !self.attached_in(shard, key) {
            return;
        }
        match reason {
            // A resize or limits change that crossed the terminal's ATTACH,
            // or a repeat: the worker keeps parsing it.
            AttachFailure::Unknown | AttachFailure::Duplicate => {}
            AttachFailure::Ring | AttachFailure::Page | AttachFailure::Parser => {
                self.hand_back(shard, vec![key], HandBack::AttachFailed(reason));
            }
        }
    }

    fn media_answer(
        &mut self,
        shard: usize,
        generation: u64,
        key: VtTerminalKey,
        answer: MediaAnswer,
    ) {
        let current = self.shards.get(shard).is_some_and(|slot| {
            slot.generation == generation && slot.worker.as_ref().is_some_and(|w| w.ready)
        });
        if current && self.attached_in(shard, key) {
            self.send(shard, Command::KittyMedia { id: key.0, answer });
        }
    }

    /// Queue a command for `shard`'s worker; a full queue marks it hung.
    fn send(&mut self, shard: usize, command: Command) {
        let slot = &mut self.shards[shard];
        let Some(worker) = &slot.worker else { return };
        if let Err(SendRefused::Full) = worker.link.send(command) {
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

    /// The worker of `shard` is gone or must go: strike the terminals whose
    /// parse was in flight, then replace it (or hand back).
    fn lost(&mut self, shard: usize, cause: LinkEnd) {
        let Some(old) = self.shards[shard].worker.take() else { return };
        let now = (self.clock)();
        let slot = &mut self.shards[shard];
        let deaths = slot.deaths.strike(now);
        let generation = slot.generation;
        slot.restore = None;
        slot.hung = false;
        let old = kill(old);
        let mut degraded = Vec::new();
        let mut crashing = Vec::new();
        for key in &self.shards[shard].members {
            let Some(terminal) = self.terminals.get_mut(key) else { continue };
            let suspect = terminal.in_flight();
            terminal.attached = false;
            terminal.answers.clear();
            terminal.answer_bytes = 0;
            terminal.next_seed = if suspect {
                match ReseedMode::after(terminal.strikes.strike(now)) {
                    Some(mode) => mode,
                    None => {
                        crashing.push(*key);
                        continue;
                    }
                }
            } else {
                ReseedMode::Full
            };
            if terminal.next_seed != ReseedMode::Full {
                degraded.push((*key, terminal.next_seed));
            }
        }
        let restart = Restart::after(deaths);
        self.host.lifecycle(WorkerLifecycle::Lost {
            shard,
            generation,
            cause,
            deaths,
            degraded,
            restart,
        });
        if !crashing.is_empty() {
            self.hand_back(shard, crashing, HandBack::TerminalCrashing);
        }
        // The successor starts before the dead worker is reaped, so the
        // restore does not wait for the reaping.
        self.schedule(shard, restart, now, now);
        reap(old);
    }

    /// The broker stops parsing `keys`; the daemon takes them back.
    fn hand_back(&mut self, shard: usize, keys: Vec<VtTerminalKey>, reason: HandBack) {
        for key in &keys {
            self.shards[shard].members.remove(key);
            if let Some(restore) = &mut self.shards[shard].restore {
                restore.pending.remove(key);
            }
            if self.terminals.remove(key).is_some_and(|terminal| terminal.attached) {
                self.send(shard, Command::Detach { id: key.0 });
            }
        }
        self.host.lifecycle(WorkerLifecycle::HandedBack {
            shard: Some(shard),
            reason,
            terminals: keys,
        });
        self.settle_restore(shard, None);
    }

    /// `shard` gets no more workers: its terminals go back to the daemon,
    /// and later attaches go to other shards.
    fn hand_back_shard(&mut self, shard: usize, reason: HandBack) {
        let slot = &mut self.shards[shard];
        slot.failed = Some(reason.clone());
        slot.relaunch = None;
        slot.restore = None;
        let worker = slot.worker.take();
        let keys: Vec<_> = std::mem::take(&mut slot.members).into_iter().collect();
        if let Some(worker) = worker {
            retire(worker);
        }
        for key in &keys {
            self.terminals.remove(key);
        }
        self.host.lifecycle(WorkerLifecycle::HandedBack {
            shard: Some(shard),
            reason,
            terminals: keys,
        });
    }
}

/// A new zeroed screen page of at least `len` bytes.
fn new_page(len: usize) -> io::Result<ScreenPage> {
    let len = len.next_multiple_of(4096).min(MAX_PAGE_BYTES);
    Ok(ScreenPage { fd: Arc::new(create_page(len)?), len: len as u64 })
}

fn event_name(event: &Event) -> &'static str {
    match event {
        Event::Hello { .. } => "HELLO",
        Event::Parsed { .. } => "PARSED",
        Event::PtyWrite { .. } => "PTY_WRITE",
        Event::Title { .. } => "TITLE",
        Event::Pwd { .. } => "PWD",
        Event::Bell { .. } => "BELL",
        Event::Lapped { .. } => "LAPPED",
        Event::PageTooSmall { .. } => "PAGE_TOO_SMALL",
        Event::AttachFailed { .. } => "ATTACH_FAILED",
        Event::ProgramStatus { .. } => "PROGRAM_STATUS",
        Event::KittyMediaRequest { .. } => "KITTY_MEDIA_REQUEST",
    }
}

/// SIGKILL the worker; its link and process are reaped by [`reap`] once
/// its successor is launched.
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
