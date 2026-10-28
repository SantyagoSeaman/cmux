//! The VT worker broker (p01-process-134) with worker processes.
//!
//! - `CMUX_TUI_VT_WORKERS=process` starts the broker
//!   (`start_process_workers`), which launches the real `cmux-vt-worker`
//!   under this platform's sandbox (Seatbelt on macOS, seccomp-bpf and
//!   landlock on Linux); the worker parses
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
//! - The app update (task 211): the broker runs the worker the platform
//!   resolves from a bundle (`worker_executable_in`: a content-addressed
//!   copy on macOS, the bundle's own file on Linux). After an update swaps
//!   the bundle, a SIGKILLed worker's successor restores every terminal to
//!   the same screens; a successor of another wire hands every terminal
//!   back to the pool threads with its screen and ring intact.
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
    BrokerConfig, Geometry, HandBack, MediaAnswer, MediaReply, MediaRequest, ProcessLauncher,
    Prologue, Reseed, ReseedMode, RingSpan, SandboxMechanism, ScreenPage, VT_WORKERS_ENV,
    VtTerminalKey, VtTerminalSpec, VtWorkerBroker, VtWorkerHost, WORKER_FD, WorkerEvent,
    WorkerLauncher, WorkerLifecycle, WorkerSandbox, start_process_workers, worker_executable_in,
};
use cmux_vt_worker::screen_page::{PageReader, Screen};
use cmux_vt_worker::wire::{
    Command, Event, FrameReader, WIRE_VERSION, WireError, decode_command, send_event,
};
use ghostty_vt::KittyGraphicsLimits;

/// The env entry that makes this binary a stand-in worker; its value is
/// the poisoned output offset.
const FAKE_WORKER_ENV: &str = "CMUX_VT_FAKE_WORKER";
/// The wire version the stand-in worker's HELLO reports (default: this one).
const FAKE_WIRE_ENV: &str = "CMUX_VT_FAKE_WIRE";
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
    let wire = std::env::var(FAKE_WIRE_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(WIRE_VERSION);
    // SAFETY: the broker's launcher gives this process its link at WORKER_FD,
    // and nothing else here owns that descriptor.
    let socket = unsafe { OwnedFd::from_raw_fd(WORKER_FD) };
    if serve_fake(&socket, poison, wire).is_err() {
        // What a parser crash looks like to the broker: the process ends.
        std::process::abort();
    }
    std::process::exit(0);
}

/// Answers the broker as the worker does, parsing nothing, with a HELLO of
/// `wire`; `Err` when it reaches `poison`.
fn serve_fake(socket: &OwnedFd, poison: Option<u64>, wire: u16) -> std::io::Result<()> {
    let fd = socket.as_raw_fd();
    let mut reader = FrameReader::default();
    let mut parsed: HashMap<u64, u64> = HashMap::new();
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
                    send_event(fd, &Event::Hello { version: wire, sandbox, pid })?;
                }
                _ if !bootstrapped => return Err(std::io::Error::other("no BOOTSTRAP first")),
                Command::Attach(attach) => {
                    parsed.insert(attach.id, attach.start);
                    let event = Event::Parsed { id: attach.id, offset: attach.start, frame: 1 };
                    send_event(fd, &event)?;
                }
                Command::Advance { id, end } => {
                    let from = parsed.get(&id).copied().unwrap_or(0);
                    if crashes(from, end) {
                        return Err(std::io::Error::other("poison"));
                    }
                    parsed.insert(id, end);
                    send_event(fd, &Event::Parsed { id, offset: end, frame: 2 })?;
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

/// Seatbelt on macOS, seccomp (with landlock where the kernel has it) on
/// Linux: never none.
fn platform_sandbox() -> SandboxMechanism {
    let required = WorkerSandbox::for_platform().expect("the platform's sandbox").required();
    assert_ne!(required, SandboxMechanism::None, "a worker always runs sandboxed");
    required
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

/// An app bundle (a package install on Linux) of the test's own:
/// `bin/cmux-vt-worker`, the platform's copy root, and the files an update
/// moves in, all written before any worker runs from them.
struct Bundle {
    base: PathBuf,
    root: PathBuf,
    worker: PathBuf,
}

impl Bundle {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("cmux-vt-worker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let worker = base.join("cmux.app/Contents/Resources/bin/cmux-vt-worker");
        std::fs::create_dir_all(worker.parent().expect("bin")).expect("the bundle");
        std::fs::create_dir_all(base.join("staged")).expect("the update's files");
        std::fs::copy(real_worker(), &worker).expect("the bundle's worker");
        Self { root: base.join("host-exe"), worker, base }
    }

    /// The next build's worker, of this wire.
    fn stage_this_wire(&self) -> PathBuf {
        let staged = self.base.join("staged/this-wire");
        std::fs::copy(real_worker(), &staged).expect("stage the next worker");
        staged
    }

    /// The next build's worker, of wire `wire`: it answers `--version`
    /// with that wire and, run as a worker, is the stand-in whose HELLO
    /// names it.
    fn stage_other_wire(&self, wire: u16) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = std::env::current_exe().expect("this test binary");
        let exe = exe.to_str().filter(|exe| !exe.contains('\'')).expect("a plain test path");
        let staged = self.base.join("staged/other-wire");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = --version ]; then echo 'cmux-vt-worker wire {wire}'; exit 0; fi\n\
             {FAKE_WORKER_ENV}=none {FAKE_WIRE_ENV}={wire} \
             exec '{exe}' --exact fake_worker_process --test-threads=1\n"
        );
        std::fs::write(&staged, script).expect("stage the other wire's worker");
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        staged
    }

    /// The update: the old bundle is deleted and a new one, with `next` as
    /// its worker, takes its place.
    fn update(&self, next: &Path) {
        std::fs::remove_dir_all(self.base.join("cmux.app")).expect("delete the old bundle");
        std::fs::create_dir_all(self.worker.parent().expect("bin")).expect("the new bundle");
        std::fs::rename(next, &self.worker).expect("the new bundle's worker");
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// One shard running the worker the platform resolves from a bundle, with
/// 16 terminals parsed.
struct Parsed {
    broker: VtWorkerBroker,
    terminals: Shared,
    events: Receiver<Seen>,
    exe: PathBuf,
    pid: u32,
    screens: Vec<Screen>,
}

fn sixteen_parsed_terminals(bundle: &Bundle) -> Parsed {
    let exe = worker_executable_in(&bundle.root, &bundle.worker)
        .expect("the bundle's worker speaks this wire");
    if cfg!(target_os = "macos") {
        assert!(exe.starts_with(&bundle.root), "macOS runs workers from a copy: {}", exe.display());
    }
    let (broker, terminals, events) = one_shard(ProcessLauncher::new(&exe));
    let mut ends = HashMap::new();
    for key in 1..=16u64 {
        let bytes = output(key);
        ends.insert(VtTerminalKey(key), bytes.len() as u64);
        broker.attach(add_terminal(&terminals, key, (0, b""), &bytes));
    }
    let pid = started(&events);
    all_parsed(&events, &ends);
    let screens = (1..=16).map(|key| screen(&terminals, key)).collect();
    Parsed { broker, terminals, events, exe, pid, screens }
}

/// SIGKILL `pid`, the shard's worker, and wait for its loss.
fn kill_worker(events: &Receiver<Seen>, pid: u32) {
    // SAFETY: kill(2) with a pid the broker started and has not reaped.
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    let lost = life(events, |event| matches!(event, WorkerLifecycle::Lost { .. }));
    assert!(
        matches!(&lost, WorkerLifecycle::Lost { deaths: 1, degraded, .. } if degraded.is_empty()),
        "{lost:?}"
    );
}

#[test]
fn after_an_app_update_a_restarted_worker_restores_every_terminal_on_the_daemons_build() {
    let bundle = Bundle::new("update");
    // macOS runs workers from the copy, so the new bundle's worker may speak
    // another wire; Linux runs the bundle's file, which a package update
    // replaces with the next build's (another wire is the hand-back below).
    let next = if cfg!(target_os = "macos") {
        bundle.stage_other_wire(WIRE_VERSION + 1)
    } else {
        bundle.stage_this_wire()
    };
    let parsed = sixteen_parsed_terminals(&bundle);
    bundle.update(&next);
    if cfg!(target_os = "macos") {
        assert_eq!(
            worker_executable_in(&bundle.root, &bundle.worker).as_deref(),
            Some(parsed.exe.as_path()),
            "the daemon keeps its own build's copy after the swap"
        );
    }
    kill_worker(&parsed.events, parsed.pid);
    let second = started(&parsed.events);
    assert_eq!(restored(&parsed.events, 16), ReseedMode::Full);
    let after: Vec<_> = (1..=16).map(|key| screen(&parsed.terminals, key)).collect();
    assert_eq!(after, parsed.screens, "no terminal lost its screen across the update");
    drop(parsed.broker);
    assert!(!alive(second), "dropping the broker ends its worker");
}

#[test]
fn a_restarted_worker_of_another_wire_hands_every_terminal_back_with_its_screen() {
    let bundle = Bundle::new("wire");
    let other = WIRE_VERSION + 1;
    let next = bundle.stage_other_wire(other);
    let parsed = sixteen_parsed_terminals(&bundle);
    // The file the next worker starts from now speaks another wire: on Linux
    // the update replaced the bundle's file, which the daemon runs; on macOS
    // it stands for a launch path that no longer holds the daemon's build.
    std::fs::rename(&next, &parsed.exe).expect("the next worker's file");
    kill_worker(&parsed.events, parsed.pid);
    started(&parsed.events);
    let handed = life(&parsed.events, |event| matches!(event, WorkerLifecycle::HandedBack { .. }));
    let WorkerLifecycle::HandedBack { shard, reason, terminals: mut keys } = handed else {
        unreachable!()
    };
    assert_eq!((shard, reason), (Some(0), HandBack::WireVersion(other)));
    keys.sort();
    assert_eq!(keys, (1..=16).map(VtTerminalKey).collect::<Vec<_>>(), "every terminal comes back");
    // The pool threads take over from the last screen and the whole ring.
    for key in 1..=16u64 {
        let index = usize::try_from(key - 1).expect("index");
        assert_eq!(
            screen(&parsed.terminals, key),
            parsed.screens[index],
            "terminal {key}'s screen"
        );
        let shared = parsed.terminals.lock().expect("terminals");
        let (ring, _) = &shared.rings[&VtTerminalKey(key)];
        assert_eq!((ring.oldest(), ring.end()), (0, output(key).len() as u64), "terminal {key}");
    }
    drop(parsed.broker);
}
