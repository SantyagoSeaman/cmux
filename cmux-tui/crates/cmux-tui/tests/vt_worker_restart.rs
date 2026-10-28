//! The VT worker broker (p01-process-134) with worker processes.
//!
//! - `CMUX_TUI_VT_WORKERS=process` starts the broker
//!   (`start_process_workers`), which launches the real `cmux-vt-worker`
//!   under this platform's sandbox (Seatbelt on macOS); the worker parses
//!   holder rings into screen pages.
//! - After a SIGKILL, the real worker's successor restores its 16
//!   terminals within 100 ms, to the same screens.
//! - A poison pill that crashes the worker twice within 10 s is restored
//!   from the prologue only. ghostty-vt has no known crashing input, so this
//!   binary is started again as a stand-in worker that speaks the worker's
//!   wire (`cmux_vt_worker::wire`), applies the policy the broker sends as
//!   the worker does, and aborts when it is asked to parse the poisoned
//!   offset.
//!
//! The real worker is the `cmux-vt-worker` cargo builds beside `cmux-tui`
//! (a `--workspace` run, as CI's, or `cargo build -p cmux-vt-worker` first),
//! or the file `CMUX_VT_WORKER_EXE` names. A missing one fails the tests.
//! Task 136 adds the other shards' echo and the ghostty-vt crash corpus.
#![cfg(unix)]

use std::collections::{BTreeSet, HashMap};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cmux_holder::ring::Ring;

use cmux_tui_core::vt_worker_runtime::{
    BrokerConfig, Geometry, MediaAnswer, MediaReply, MediaRequest, ProcessLauncher, Prologue,
    Reseed, ReseedMode, RingSpan, SandboxMechanism, ScreenPage, VT_WORKERS_ENV, VtTerminalKey,
    VtTerminalSpec, VtWorkerBroker, VtWorkerHost, WORKER_FD, WorkerEvent, WorkerLauncher,
    WorkerLifecycle, WorkerSandbox, start_process_workers,
};
use cmux_vt_worker::screen_page::{PageReader, Screen};
use cmux_vt_worker::wire::{
    Command, Event, FrameReader, WIRE_VERSION, WireError, decode_command, send_event,
};
use ghostty_vt::KittyGraphicsLimits;

/// The env entry that makes this binary a stand-in worker; its value is
/// the poisoned output offset.
const FAKE_WORKER_ENV: &str = "CMUX_VT_FAKE_WORKER";
/// The env entry that names the real worker instead of the one beside
/// `cmux-tui`.
const WORKER_EXE_ENV: &str = "CMUX_VT_WORKER_EXE";
/// How long a test waits for an event before it fails.
const PATIENCE: Duration = Duration::from_secs(10);
/// The acceptance bound for a restore after a SIGKILL.
const RESTORE_BOUND: Duration = Duration::from_millis(100);
const GEOMETRY: Geometry = Geometry { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };
const RING_CAPACITY: usize = 1 << 20;

/// Not a test: the body of the stand-in worker. Run by the harness without
/// [`FAKE_WORKER_ENV`], it does nothing.
#[test]
fn fake_worker_process() {
    let Some(poison) = std::env::var_os(FAKE_WORKER_ENV) else { return };
    let poison: Option<u64> = poison.to_str().and_then(|value| value.parse().ok());
    // SAFETY: the broker's launcher gives this process its link at WORKER_FD,
    // and nothing else here owns that descriptor.
    let socket = unsafe { OwnedFd::from_raw_fd(WORKER_FD) };
    if serve_fake(&socket, poison).is_err() {
        // What a parser crash looks like to the broker: the process ends.
        std::process::abort();
    }
    std::process::exit(0);
}

/// Answers the broker as the worker does, parsing nothing; `Err` when it
/// reaches `poison`.
fn serve_fake(socket: &OwnedFd, poison: Option<u64>) -> std::io::Result<()> {
    let fd = socket.as_raw_fd();
    let mut reader = FrameReader::default();
    let mut parsed: HashMap<u64, u64> = HashMap::new();
    // Each publish is newer than the last (the bad_message rule).
    let mut publishes = 0;
    let crashes = |from: u64, to: u64| poison.is_some_and(|poison| (from..to).contains(&poison));
    let bad = |error: WireError| std::io::Error::other(error.to_string());
    let mut bootstrapped = false;
    loop {
        while let Some(frame) = reader.next_frame().map_err(bad)? {
            match decode_command(frame).map_err(bad)? {
                Command::Bootstrap(policy) => {
                    // The policy the real worker gets, applied as it does.
                    let sandbox = cmux_vt_worker::sandbox::apply(&policy)
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                    bootstrapped = true;
                    let pid = std::process::id();
                    send_event(fd, &Event::Hello { version: WIRE_VERSION, sandbox, pid })?;
                }
                _ if !bootstrapped => return Err(std::io::Error::other("no BOOTSTRAP first")),
                Command::Attach(attach) => {
                    parsed.insert(attach.id, attach.start);
                    publishes += 1;
                    let event =
                        Event::Parsed { id: attach.id, offset: attach.start, frame: publishes };
                    send_event(fd, &event)?;
                }
                Command::Advance { id, end } => {
                    let from = parsed.get(&id).copied().unwrap_or(0);
                    if crashes(from, end) {
                        return Err(std::io::Error::other("poison"));
                    }
                    parsed.insert(id, end);
                    publishes += 1;
                    send_event(fd, &Event::Parsed { id, offset: end, frame: publishes })?;
                }
                _ => {}
            }
        }
        if reader.fill(fd)? == 0 {
            return Ok(());
        }
    }
}

/// The holder side of the test's terminals: their rings and prologues, and
/// the pages the broker made for them.
#[derive(Default)]
struct Terminals {
    rings: HashMap<VtTerminalKey, (Ring, Prologue)>,
    pages: HashMap<VtTerminalKey, ScreenPage>,
}

type Shared = Arc<Mutex<Terminals>>;

#[derive(Debug)]
enum Seen {
    Parsed(VtTerminalKey, u64),
    Life(WorkerLifecycle),
}

struct Host {
    terminals: Shared,
    events: Sender<Seen>,
}

impl VtWorkerHost for Host {
    fn reseed(&mut self, terminal: VtTerminalKey) -> Option<Reseed> {
        let terminals = self.terminals.lock().expect("terminals");
        let (ring, prologue) = terminals.rings.get(&terminal)?;
        Some(Reseed {
            prologue: prologue.clone(),
            ring: RingSpan { oldest: ring.oldest(), end: ring.end() },
        })
    }

    fn write_answer(&mut self, _terminal: VtTerminalKey, _answer: &[u8]) {}

    fn parsed(&mut self, terminal: VtTerminalKey, offset: u64) {
        let _ = self.events.send(Seen::Parsed(terminal, offset));
    }

    fn screen_page(&mut self, terminal: VtTerminalKey, page: &ScreenPage) {
        self.terminals.lock().expect("terminals").pages.insert(terminal, page.clone());
    }

    fn terminal_event(&mut self, _terminal: VtTerminalKey, _event: WorkerEvent) {}

    fn kitty_media(&mut self, _terminal: VtTerminalKey, request: MediaRequest, reply: MediaReply) {
        if let Ok(answer) = MediaAnswer::read(&request) {
            reply.send(answer);
        }
    }

    fn lifecycle(&mut self, event: WorkerLifecycle) {
        let _ = self.events.send(Seen::Life(event));
    }
}

fn host() -> (Box<Host>, Shared, Receiver<Seen>) {
    let terminals = Shared::default();
    let (events, received) = channel();
    (Box::new(Host { terminals: terminals.clone(), events }), terminals, received)
}

/// A broker of one shard that launches with `launcher`.
fn one_shard(launcher: impl WorkerLauncher) -> (VtWorkerBroker, Shared, Receiver<Seen>) {
    let (host, terminals, events) = host();
    let config = BrokerConfig { shards: 1, ..BrokerConfig::for_machine() };
    let sandbox = WorkerSandbox::for_platform().expect("the platform's sandbox");
    let broker =
        VtWorkerBroker::start(config, sandbox, Box::new(launcher), host, Box::new(Instant::now))
            .expect("start the broker");
    (broker, terminals, events)
}

/// A terminal whose ring holds `output` and whose prologue is `prologue`
/// at offset `base`.
fn add_terminal(
    terminals: &Shared,
    key: u64,
    (base, prologue): (u64, &[u8]),
    output: &[u8],
) -> VtTerminalSpec {
    let mut ring = Ring::new(0, RING_CAPACITY);
    ring.push(output);
    let segment = ring.segment().expect("a shared ring").try_clone().expect("dup ring");
    let prologue = Prologue { base, bytes: prologue.to_vec() };
    terminals.lock().expect("terminals").rings.insert(VtTerminalKey(key), (ring, prologue));
    VtTerminalSpec {
        key: VtTerminalKey(key),
        geometry: GEOMETRY,
        ring: segment,
        ring_capacity: RING_CAPACITY as u64,
        scrollback: 1 << 20,
        kitty_limits: KittyGraphicsLimits::default(),
        colors: None,
        answered: 0,
    }
}

/// The screen the worker published for `key`, without its publish count.
fn screen(terminals: &Shared, key: u64) -> Screen {
    let page = terminals.lock().expect("terminals").pages[&VtTerminalKey(key)].clone();
    let len = usize::try_from(page.len).expect("page length");
    let mut screen = PageReader::open(&page.fd, len).expect("map the page").read().expect("read");
    screen.frame = 0;
    screen
}

fn wait_for(events: &Receiver<Seen>, mut wanted: impl FnMut(&Seen) -> bool) -> Seen {
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

fn life(
    events: &Receiver<Seen>,
    mut wanted: impl FnMut(&WorkerLifecycle) -> bool,
) -> WorkerLifecycle {
    match wait_for(events, |event| matches!(event, Seen::Life(life) if wanted(life))) {
        Seen::Life(life) => life,
        Seen::Parsed(..) => unreachable!(),
    }
}

fn started(events: &Receiver<Seen>) -> u32 {
    match life(events, |event| matches!(event, WorkerLifecycle::Started { .. })) {
        WorkerLifecycle::Started { pid, .. } => pid,
        _ => unreachable!(),
    }
}

/// The restore that ends with `terminals` parsed: its mode.
fn restored(events: &Receiver<Seen>, terminals: usize) -> ReseedMode {
    match life(
        events,
        |event| matches!(event, WorkerLifecycle::Restored { terminals: n, .. } if *n == terminals),
    ) {
        WorkerLifecycle::Restored { mode, .. } => mode,
        _ => unreachable!(),
    }
}

/// Waits until every terminal in `ends` reported parsing to its end.
fn all_parsed(events: &Receiver<Seen>, ends: &HashMap<VtTerminalKey, u64>) {
    let mut left: BTreeSet<_> = ends.keys().copied().collect();
    while !left.is_empty() {
        if let Seen::Parsed(key, offset) =
            wait_for(events, |event| matches!(event, Seen::Parsed(..)))
            && ends.get(&key).is_some_and(|end| offset >= *end)
        {
            left.remove(&key);
        }
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// The real `cmux-vt-worker` (module docs).
fn real_worker() -> PathBuf {
    if let Some(path) = std::env::var_os(WORKER_EXE_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    let path = Path::new(env!("CARGO_BIN_EXE_cmux-tui")).with_file_name("cmux-vt-worker");
    assert!(
        path.is_file(),
        "{} is not built: run the tests with --workspace, or `cargo build -p cmux-vt-worker` \
         first, or set {WORKER_EXE_ENV}",
        path.display()
    );
    path
}

/// Output that fills a screen: a title, colored lines and a cursor move.
fn output(key: u64) -> Vec<u8> {
    let mut bytes = format!("\x1b]0;terminal {key}\x07").into_bytes();
    for line in 0..40 {
        let color = line % 8;
        bytes.extend_from_slice(
            format!("\x1b[3{color}mterminal {key} line {line}\x1b[0m\r\n").as_bytes(),
        );
    }
    bytes.extend_from_slice(format!("\x1b[5;10H\x1b[1mcursor of {key}").as_bytes());
    bytes
}

fn platform_sandbox() -> SandboxMechanism {
    if cfg!(target_os = "macos") { SandboxMechanism::Seatbelt } else { SandboxMechanism::None }
}

#[test]
fn the_env_switch_starts_real_sandboxed_workers_that_parse_holder_rings() {
    let (off, _, _) = host();
    assert!(
        start_process_workers(None, real_worker(), off).expect("start").is_none(),
        "without {VT_WORKERS_ENV}=process there is no broker"
    );
    let (host, terminals, events) = host();
    let broker = start_process_workers(Some("process"), real_worker(), host)
        .expect("start the broker")
        .expect("process asks for workers");
    let mut ends = HashMap::new();
    for key in 1..=4u64 {
        let bytes = output(key);
        ends.insert(VtTerminalKey(key), bytes.len() as u64);
        broker.attach(add_terminal(&terminals, key, (0, b""), &bytes));
    }
    let pid = started(&events);
    let ready = life(&events, |event| matches!(event, WorkerLifecycle::Ready { .. }));
    assert!(
        matches!(ready, WorkerLifecycle::Ready { sandbox, .. } if sandbox == platform_sandbox()),
        "{ready:?}"
    );
    all_parsed(&events, &ends);
    for key in 1..=4u64 {
        let screen = screen(&terminals, key);
        assert_eq!(screen.parsed, ends[&VtTerminalKey(key)]);
        let text: String =
            screen.cells.iter().flatten().filter_map(|cell| cell.text.chars().next()).collect();
        assert!(text.contains(&format!("cursor of {key}")), "{text}");
    }
    drop(broker);
    assert!(!alive(pid), "dropping the broker ends its workers");
}

#[test]
fn a_sigkilled_worker_restores_its_16_terminals_within_100_ms_to_the_same_screens() {
    let (broker, terminals, events) = one_shard(ProcessLauncher::new(real_worker()));
    let mut ends = HashMap::new();
    for key in 1..=16u64 {
        let bytes = output(key);
        ends.insert(VtTerminalKey(key), bytes.len() as u64);
        broker.attach(add_terminal(&terminals, key, (0, b""), &bytes));
    }
    let first = started(&events);
    all_parsed(&events, &ends);
    let before: Vec<_> = (1..=16).map(|key| screen(&terminals, key)).collect();
    let killed = Instant::now();
    // SAFETY: kill(2) with a pid the broker started and has not reaped.
    assert_eq!(unsafe { libc::kill(first as libc::pid_t, libc::SIGKILL) }, 0);
    let lost = life(&events, |event| matches!(event, WorkerLifecycle::Lost { .. }));
    assert!(
        matches!(&lost, WorkerLifecycle::Lost { deaths: 1, degraded, .. } if degraded.is_empty()),
        "{lost:?}"
    );
    let second = started(&events);
    let mode = restored(&events, 16);
    let elapsed = killed.elapsed();
    assert_eq!(mode, ReseedMode::Full);
    assert!(elapsed < RESTORE_BOUND, "16 terminals restored {elapsed:?} after the SIGKILL");
    assert_ne!(first, second);
    assert!(!alive(first), "the killed worker was reaped");
    let after: Vec<_> = (1..=16).map(|key| screen(&terminals, key)).collect();
    assert_eq!(after, before, "the successor's screens are the killed worker's");
    drop(broker);
    assert!(!alive(second), "dropping the broker ends its worker");
}

#[test]
fn a_poison_pill_crashing_twice_in_10_s_restores_from_the_prologue_only() {
    let exe = std::env::current_exe().expect("this test binary");
    // The poison is in the ring tail after the prologue: every full seed
    // replays it and crashes.
    let launcher = ProcessLauncher::new(exe)
        .arg("--exact")
        .arg("fake_worker_process")
        .arg("--test-threads=1")
        .env(FAKE_WORKER_ENV, "300");
    let (broker, terminals, events) = one_shard(launcher);
    broker.attach(add_terminal(&terminals, 1, (100, b"\x1b[H\x1b[2Jprologue"), &[b'x'; 400]));
    let poisoned = VtTerminalKey(1);
    for (deaths, degraded) in [(1, vec![]), (2, vec![(poisoned, ReseedMode::PrologueOnly)])] {
        let lost = life(&events, |event| matches!(event, WorkerLifecycle::Lost { .. }));
        assert!(
            matches!(&lost, WorkerLifecycle::Lost { deaths: d, degraded: g, .. } if *d == deaths && *g == degraded),
            "{lost:?}"
        );
    }
    assert_eq!(restored(&events, 1), ReseedMode::PrologueOnly);
    // The third worker lives: no further loss.
    let deadline = Instant::now() + Duration::from_millis(200);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(Seen::Life(event @ WorkerLifecycle::Lost { .. })) => {
                panic!("the prologue-only worker crashed too: {event:?}")
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    drop(broker);
}
