//! The VT worker broker (p01-process-134) with real worker processes that
//! parse with the real ghostty-vt (task 136). This test binary, started
//! again by the broker's `ProcessLauncher`, is the worker: it speaks the
//! broker's wire, takes each terminal's ring as a descriptor, parses its
//! seed and advances with `ghostty_vt::Terminal`, and records a digest of
//! each screen (text with scrollback, cursor, title) before it reports a
//! terminal parsed. The test compares those digests with the same bytes
//! parsed in this process.
//!
//! - 16 terminals in one shard: a SIGKILLed worker is replaced, its 16
//!   terminals are restored within 100 ms, and every screen digest equals
//!   the one before the kill, plus the output written during the restore.
//! - Two shards: the other shard's echo (ring advance to parsed) during the
//!   restart stays within 5 ms at p99.
//! - A poison pill that crashes the worker twice within 10 s restores from
//!   the prologue only, and the third worker lives.
//! - A crash corpus (the ways a broken parser ends its worker: SIGSEGV,
//!   SIGABRT, SIGBUS, a stack overflow, an exit, a cut frame, a garbage
//!   frame) ends only the shard whose terminal carries it. The bystander
//!   terminals carry ghostty's own fuzz stream corpus, and the worker's
//!   digests of it equal this process's.
//!
//! Each case is red without the broker's reseed: a new worker that gets no
//! seed never restores, and one seeded from the wrong offset has another
//! digest.
#![cfg(unix)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use cmux_ipc::fds::FdReceiver;
use cmux_tui_core::vt_worker_runtime::wire::{
    Frame, FrameReader, Kind, MAGIC, SeedFrame, WIRE_VERSION, decode_offset, decode_size,
};
use cmux_tui_core::vt_worker_runtime::{
    BrokerConfig, LinkEnd, ProcessLauncher, Prologue, Reseed, ReseedMode, RingSpan, VtTerminalKey,
    VtTerminalSpec, VtWorkerBroker, VtWorkerHost, WORKER_FD, WorkerLifecycle,
};
use ghostty_vt::{Callbacks, Terminal};
use sha2::{Digest as _, Sha256};

/// The env entry that makes this binary a worker; its value is the
/// directory the worker writes its screen digests to.
const WORKER_ENV: &str = "CMUX_VT_TEST_WORKER";
/// How long a test waits for an event before it fails.
const PATIENCE: Duration = Duration::from_secs(10);
/// The acceptance bound for a restore after a SIGKILL.
const RESTORE_BOUND: Duration = Duration::from_millis(100);
/// The acceptance bound for another shard's echo p99 during a restart.
const ECHO_P99_BOUND: Duration = Duration::from_millis(5);
const COLS: u16 = 80;
const ROWS: u16 = 24;
/// Retained scrollback per screen, in bytes.
const SCROLLBACK: usize = 1 << 20;
/// What a worker writes after a skipped stretch of output: CAN ends any
/// sequence the prologue left open (the wire's `FLAG_SKIPPED`).
const CANCEL: &[u8] = b"\x18";
/// A crash marker in a ring: `ESC ] 9999 ; crash= <kind> BEL`. The worker
/// crashes the way `<kind>` names before it parses the chunk holding it.
const CRASH_MARK: &[u8] = b"\x1b]9999;crash=";
/// The largest bystander ring built from ghostty's fuzz corpus.
const CORPUS_RING: usize = 256 * 1024;

// The worker.

/// Not a test: the body of the worker process. Run by the harness without
/// [`WORKER_ENV`], it does nothing.
#[test]
fn vt_worker_process() {
    let Some(dir) = std::env::var_os(WORKER_ENV) else { return };
    // SAFETY: the broker's launcher gives this process its link at
    // WORKER_FD, and nothing else here owns that descriptor.
    let link = unsafe { OwnedFd::from_raw_fd(WORKER_FD) };
    let code = match serve(UnixStream::from(link), Path::new(&dir)) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    std::process::exit(code);
}

/// The link as a byte stream that keeps the descriptors arriving with it.
struct LinkIn {
    stream: UnixStream,
    fds: Rc<RefCell<FdReceiver>>,
}

impl Read for LinkIn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fds.borrow_mut().recv_into(self.stream.as_raw_fd(), buf, |_| {})
    }
}

struct WorkerTerminal {
    ring: File,
    cols: u16,
    rows: u16,
    screen: Option<Screen>,
    parsed: u64,
}

fn serve(stream: UnixStream, dir: &Path) -> io::Result<()> {
    let mut out = stream.try_clone()?;
    let fds = Rc::new(RefCell::new(FdReceiver::default()));
    let mut reader = FrameReader::new(LinkIn { stream, fds: fds.clone() });
    let mut terminals: HashMap<u64, WorkerTerminal> = HashMap::new();
    let pid = std::process::id();
    while let Some(frame) = reader.read_frame()? {
        let key = frame.terminal;
        match frame.kind {
            Kind::Hello => out.write_all(&Frame::ready(WIRE_VERSION).encode())?,
            Kind::Assign => {
                let (cols, rows) = decode_size(&frame)?;
                let ring = fds
                    .borrow_mut()
                    .take(1)
                    .map_err(|error| error.into_io(io::ErrorKind::InvalidData))?
                    .pop()
                    .ok_or_else(|| io::Error::other("ASSIGN without its ring"))?;
                let ring = File::from(ring);
                terminals.insert(key, WorkerTerminal { ring, cols, rows, screen: None, parsed: 0 });
            }
            Kind::Seed => {
                let seed = SeedFrame::decode(&frame)?;
                let Some(terminal) = terminals.get_mut(&key) else { continue };
                let mut screen =
                    Screen::seeded(terminal.cols, terminal.rows, seed.prologue, seed.skipped);
                let bytes = read_ring(&terminal.ring, seed.replay_from, seed.end)?;
                crash_if_marked(&bytes, &mut out)?;
                screen.write(&bytes);
                terminal.screen = Some(screen);
                terminal.parsed = seed.end;
                report(dir, pid, key, terminal, &mut out)?;
            }
            Kind::Advance => {
                let end = decode_offset(&frame)?;
                let Some(terminal) = terminals.get_mut(&key) else { continue };
                if end > terminal.parsed {
                    let bytes = read_ring(&terminal.ring, terminal.parsed, end)?;
                    crash_if_marked(&bytes, &mut out)?;
                    if let Some(screen) = &mut terminal.screen {
                        screen.write(&bytes);
                    }
                    terminal.parsed = end;
                }
                report(dir, pid, key, terminal, &mut out)?;
            }
            Kind::Resize => {
                let (cols, rows) = decode_size(&frame)?;
                let Some(terminal) = terminals.get_mut(&key) else { continue };
                (terminal.cols, terminal.rows) = (cols, rows);
                if let Some(screen) = &mut terminal.screen {
                    screen.resize(cols, rows);
                }
            }
            Kind::Release => {
                terminals.remove(&key);
            }
            Kind::Ready | Kind::Parsed | Kind::Answer => {}
        }
    }
    Ok(())
}

fn read_ring(ring: &File, from: u64, to: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(to.saturating_sub(from)).map_err(io::Error::other)?;
    let mut bytes = vec![0; len];
    ring.read_exact_at(&mut bytes, from)?;
    Ok(bytes)
}

/// Record the terminal's screen digest, then tell the broker it parsed.
fn report(
    dir: &Path,
    pid: u32,
    key: u64,
    terminal: &mut WorkerTerminal,
    out: &mut UnixStream,
) -> io::Result<()> {
    let digest = terminal.screen.as_mut().map(Screen::digest).unwrap_or_default();
    let record = format!("{pid} {} {digest}\n", terminal.parsed);
    // Written whole, then renamed: the test never reads half a record.
    let partial = dir.join(format!("{key}.{pid}.partial"));
    std::fs::write(&partial, record)?;
    std::fs::rename(&partial, dir.join(key.to_string()))?;
    out.write_all(&Frame::parsed(key, terminal.parsed).encode())
}

/// The crash corpus: each way a parser crash ends a worker.
const CRASH_CORPUS: [&str; 7] =
    ["segv", "abort", "bus", "stack-overflow", "exit", "cut-frame", "garbage-frame"];

fn crash_marker(kind: &str) -> Vec<u8> {
    [CRASH_MARK, kind.as_bytes(), b"\x07"].concat()
}

/// Crash as the first marker in `bytes` names, if there is one.
fn crash_if_marked(bytes: &[u8], out: &mut UnixStream) -> io::Result<()> {
    let Some(at) = bytes.windows(CRASH_MARK.len()).position(|window| window == CRASH_MARK) else {
        return Ok(());
    };
    let rest = bytes.get(at + CRASH_MARK.len()..).unwrap_or_default();
    let kind = rest.split(|&byte| byte == 0x07).next().unwrap_or_default();
    match kind {
        // SAFETY: raise(3) with a valid signal number; its default action
        // ends this process, which is the point.
        b"segv" => unsafe { libc::raise(libc::SIGSEGV) },
        b"abort" => std::process::abort(),
        // SAFETY: as above.
        b"bus" => unsafe { libc::raise(libc::SIGBUS) },
        b"stack-overflow" => return Err(io::Error::other(overflow(0).to_string())),
        b"exit" => std::process::exit(3),
        b"cut-frame" => {
            let parsed = Frame::parsed(1, 1).encode();
            out.write_all(parsed.get(..parsed.len() / 2).unwrap_or_default())?;
            std::process::exit(0);
        }
        b"garbage-frame" => {
            // The broker ends a live worker that sends this: keep serving.
            let mut garbage = Frame::parsed(1, 1).encode();
            garbage.get_mut(..MAGIC.len()).unwrap_or_default().copy_from_slice(b"XXXX");
            out.write_all(&garbage)?;
            return Ok(());
        }
        _ => return Err(io::Error::other("unknown crash kind")),
    };
    Err(io::Error::other("the crash did not end the worker"))
}

/// Recurses until the stack's guard page ends the process.
fn overflow(depth: u64) -> u64 {
    let frame = std::hint::black_box([depth; 64]);
    if std::hint::black_box(depth) == u64::MAX {
        return frame.iter().sum();
    }
    overflow(depth + 1).wrapping_add(frame.iter().sum())
}

// The screen both sides parse into.

struct Screen {
    terminal: Terminal,
}

impl Screen {
    fn new(cols: u16, rows: u16) -> Self {
        let terminal = Terminal::new(cols, rows, SCROLLBACK, Callbacks::default())
            .expect("a ghostty terminal");
        Self { terminal }
    }

    /// A screen as a worker seeds it: the prologue, then CAN when output
    /// after it is skipped.
    fn seeded(cols: u16, rows: u16, prologue: &[u8], skipped: bool) -> Self {
        let mut screen = Self::new(cols, rows);
        screen.write(prologue);
        if skipped {
            screen.write(CANCEL);
        }
        screen
    }

    fn write(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.terminal.resize(cols, rows, 8, 16);
    }

    /// SHA-256 of the text with scrollback, the cursor and the title.
    fn digest(&mut self) -> String {
        let text = self.terminal.plain_text().unwrap_or_default();
        let cursor = self.terminal.cursor_position();
        let title = self.terminal.title().unwrap_or_default();
        let mut hash = Sha256::new();
        hash.update(text.as_bytes());
        hash.update(format!("\0{cursor:?}\0{title}").as_bytes());
        hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

// The broker side.

/// One terminal's output: `history` is what its prologue replays (the ring
/// before the prologue's base), `tail` the ring after it.
struct Setup {
    history: Vec<u8>,
    tail: Vec<u8>,
}

impl Setup {
    fn plain(tail: Vec<u8>) -> Self {
        Self { history: Vec::new(), tail }
    }
}

/// What the host reads on the broker thread.
struct Shared {
    prologues: Vec<Prologue>,
    ends: Vec<AtomicU64>,
}

impl Shared {
    fn index(key: VtTerminalKey) -> usize {
        usize::try_from(key.0 - 1).expect("a small key")
    }
}

struct Host {
    shared: Arc<Shared>,
    events: Sender<WorkerLifecycle>,
    parsed: Sender<(VtTerminalKey, u64)>,
}

impl VtWorkerHost for Host {
    fn reseed(&mut self, terminal: VtTerminalKey) -> Option<Reseed> {
        let index = Shared::index(terminal);
        let prologue = self.shared.prologues.get(index)?.clone();
        let end = self.shared.ends.get(index)?.load(Ordering::Acquire);
        Some(Reseed { prologue, ring: RingSpan { oldest: 0, end } })
    }

    fn write_answer(&mut self, _terminal: VtTerminalKey, _answer: &[u8]) {}

    fn parsed(&mut self, terminal: VtTerminalKey, offset: u64) {
        let _ = self.parsed.send((terminal, offset));
    }

    fn lifecycle(&mut self, event: WorkerLifecycle) {
        let _ = self.events.send(event);
    }
}

/// A worker's record of one terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    pid: u32,
    offset: u64,
    digest: String,
}

struct Fixture {
    broker: Option<VtWorkerBroker>,
    events: Receiver<WorkerLifecycle>,
    parsed: Receiver<(VtTerminalKey, u64)>,
    shared: Arc<Shared>,
    rings: Vec<(File, Vec<u8>)>,
    digests: tempfile::TempDir,
    /// Every lifecycle event seen, in order.
    log: Vec<WorkerLifecycle>,
    /// Which events of `log` a [`Self::wait_for`] returned.
    taken: Vec<bool>,
}

impl Fixture {
    fn start(shards: usize, setups: Vec<Setup>) -> Self {
        let digests = tempfile::tempdir().expect("a digest directory");
        let mut prologues = Vec::new();
        let mut rings = Vec::new();
        for Setup { history, tail } in setups {
            let mut screen = Screen::new(COLS, ROWS);
            screen.write(&history);
            let bytes = if history.is_empty() {
                Vec::new()
            } else {
                screen.terminal.vt_replay_bytes().expect("a prologue")
            };
            prologues.push(Prologue { base: history.len() as u64, bytes });
            let content = [history, tail].concat();
            let mut file = tempfile::tempfile().expect("a ring file");
            file.write_all(&content).expect("write the ring");
            rings.push((file, content));
        }
        let ends = rings.iter().map(|(_, content)| AtomicU64::new(content.len() as u64)).collect();
        let shared = Arc::new(Shared { prologues, ends });
        let exe = std::env::current_exe().expect("this test binary");
        let launcher = ProcessLauncher::new(exe)
            .arg("--exact")
            .arg("vt_worker_process")
            .arg("--test-threads=1")
            .env(WORKER_ENV, digests.path());
        let (events_tx, events) = channel();
        let (parsed_tx, parsed) = channel();
        let host = Host { shared: shared.clone(), events: events_tx, parsed: parsed_tx };
        let broker = VtWorkerBroker::start(
            BrokerConfig { shards, queue: 256 },
            Box::new(launcher),
            Box::new(host),
            Box::new(Instant::now),
        )
        .expect("start the broker");
        for (index, (file, _)) in rings.iter().enumerate() {
            let ring = OwnedFd::from(file.try_clone().expect("dup the ring"));
            let key = VtTerminalKey(index as u64 + 1);
            broker.attach(VtTerminalSpec { key, cols: COLS, rows: ROWS, ring });
        }
        Self {
            broker: Some(broker),
            events,
            parsed,
            shared,
            rings,
            digests,
            log: Vec::new(),
            taken: Vec::new(),
        }
    }

    fn broker(&self) -> &VtWorkerBroker {
        self.broker.as_ref().expect("a running broker")
    }

    fn keys(&self) -> impl Iterator<Item = VtTerminalKey> + use<> {
        (1..=self.rings.len() as u64).map(VtTerminalKey)
    }

    fn end(&self, key: VtTerminalKey) -> u64 {
        self.shared.ends[Shared::index(key)].load(Ordering::Acquire)
    }

    /// Write `bytes` to the terminal's ring and tell the broker.
    fn append(&mut self, key: VtTerminalKey, bytes: &[u8]) -> u64 {
        let index = Shared::index(key);
        let (file, content) = &mut self.rings[index];
        file.write_all(bytes).expect("append to the ring");
        content.extend_from_slice(bytes);
        let end = content.len() as u64;
        self.shared.ends[index].store(end, Ordering::Release);
        self.broker().advance(key, end);
        end
    }

    /// Wait until the broker confirms `key` parsed up to `offset`.
    fn wait_parsed(&self, key: VtTerminalKey, offset: u64) {
        let deadline = Instant::now() + PATIENCE;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.parsed.recv_timeout(left) {
                Ok((got, reached)) if got == key && reached >= offset => return,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        panic!("{key:?} not parsed to {offset} within {PATIENCE:?}");
    }

    /// The first event `wanted` takes that no earlier wait returned, in
    /// arrival order; every event goes to the log.
    fn wait_for(&mut self, mut wanted: impl FnMut(&WorkerLifecycle) -> bool) -> WorkerLifecycle {
        let logged =
            (0..self.log.len()).find(|&index| !self.taken[index] && wanted(&self.log[index]));
        if let Some(index) = logged {
            self.taken[index] = true;
            return self.log[index].clone();
        }
        let deadline = Instant::now() + PATIENCE;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(event) => {
                    let found = wanted(&event);
                    self.log.push(event.clone());
                    self.taken.push(found);
                    if found {
                        return event;
                    }
                }
                Err(_) => break,
            }
        }
        panic!("no wanted event within {PATIENCE:?}; saw {:#?}", self.log);
    }

    /// Log the events that arrived; whether one of them is `wanted`.
    fn drain(&mut self, mut wanted: impl FnMut(&WorkerLifecycle) -> bool) -> bool {
        let mut found = false;
        while let Ok(event) = self.events.try_recv() {
            found |= wanted(&event);
            self.log.push(event);
            self.taken.push(false);
        }
        found
    }

    fn started(&mut self, shard: usize) -> u32 {
        match self.wait_for(
            |event| matches!(event, WorkerLifecycle::Started { shard: s, .. } if *s == shard),
        ) {
            WorkerLifecycle::Started { pid, .. } => pid,
            _ => unreachable!(),
        }
    }

    /// The restore of `shard` that ends with `terminals` parsed.
    fn restored(&mut self, shard: usize, terminals: usize) -> (ReseedMode, Duration) {
        match self.wait_for(|event| {
            matches!(event, WorkerLifecycle::Restored { shard: s, terminals: n, .. }
                if *s == shard && *n == terminals)
        }) {
            WorkerLifecycle::Restored { mode, elapsed, .. } => (mode, elapsed),
            _ => unreachable!(),
        }
    }

    fn lost(&mut self, shard: usize) -> WorkerLifecycle {
        self.wait_for(
            |event| matches!(event, WorkerLifecycle::Lost { shard: s, .. } if *s == shard),
        )
    }

    /// The pid of `shard`'s newest worker.
    fn worker_pid(&self, shard: usize) -> u32 {
        self.log
            .iter()
            .rev()
            .find_map(|event| match event {
                WorkerLifecycle::Started { shard: s, pid, .. } if *s == shard => Some(*pid),
                _ => None,
            })
            .expect("a started worker")
    }

    /// The shard whose worker wrote `key`'s record.
    fn shard_of(&self, key: VtTerminalKey) -> usize {
        let pid = self.record(key).pid;
        self.log
            .iter()
            .find_map(|event| match event {
                WorkerLifecycle::Started { shard, pid: p, .. } if *p == pid => Some(*shard),
                _ => None,
            })
            .expect("the record's worker started")
    }

    fn lost_any(&self, shard: usize) -> bool {
        self.log
            .iter()
            .any(|event| matches!(event, WorkerLifecycle::Lost { shard: s, .. } if *s == shard))
    }

    fn record(&self, key: VtTerminalKey) -> Record {
        let path: PathBuf = self.digests.path().join(key.0.to_string());
        let text = std::fs::read_to_string(&path).expect("a worker record");
        let mut fields = text.split_whitespace();
        let pid = fields.next().and_then(|pid| pid.parse().ok()).expect("a pid");
        let offset = fields.next().and_then(|offset| offset.parse().ok()).expect("an offset");
        let digest = fields.next().unwrap_or_default().to_owned();
        Record { pid, offset, digest }
    }

    /// The digest of a full reseed: the prologue, then the ring after it.
    fn full_digest(&self, key: VtTerminalKey) -> String {
        let index = Shared::index(key);
        let prologue = &self.shared.prologues[index];
        let content = &self.rings[index].1;
        let mut screen = Screen::seeded(COLS, ROWS, &prologue.bytes, false);
        screen.write(&content[prologue.base as usize..]);
        screen.digest()
    }

    /// The digest of a prologue-only reseed.
    fn prologue_digest(&self, key: VtTerminalKey) -> String {
        let prologue = &self.shared.prologues[Shared::index(key)];
        Screen::seeded(COLS, ROWS, &prologue.bytes, self.end(key) > prologue.base).digest()
    }

    /// `key`'s record is `pid`'s, at the ring's end, with `digest`.
    fn assert_record(&self, key: VtTerminalKey, pid: u32, digest: &str) {
        let record = self.record(key);
        assert_eq!(
            record,
            Record { pid, offset: self.end(key), digest: digest.to_owned() },
            "{key:?}"
        );
    }

    fn kill(&self, pid: u32) {
        // SAFETY: kill(2) with a pid the broker started and has not reaped.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    }

    /// One echo: a byte to `key`'s ring until the broker confirms it parsed.
    fn echo(&mut self, key: VtTerminalKey) -> Duration {
        let started = Instant::now();
        let end = self.append(key, b".");
        self.wait_parsed(key, end);
        started.elapsed()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The broker ends its workers before the digest directory goes.
        self.broker = None;
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Lines with colors, a cursor move and a title: what a shell leaves.
fn shell_output(key: u64, from: usize, lines: usize) -> Vec<u8> {
    let mut out = format!("\x1b]2;terminal {key}\x07").into_bytes();
    for line in from..from + lines {
        let color = 31 + (line % 7);
        out.extend(format!("\x1b[{color}mterminal {key} line {line}\x1b[0m $ ls\r\n").bytes());
    }
    out.extend(format!("\x1b[5;10H\x1b[1mmark {key}\x1b[0m\x1b[{ROWS};1H").bytes());
    out
}

/// A full-screen program on the alternate screen.
fn editor_output(key: u64) -> Vec<u8> {
    let mut out = b"\x1b[?1049h\x1b[H\x1b[2J".to_vec();
    for row in 1..=ROWS {
        out.extend(format!("\x1b[{row};1H~ {key}:{row}").bytes());
    }
    out.extend(b"\x1b[7m -- INSERT -- \x1b[0m\x1b[3;5H");
    out
}

/// `count` terminals: shell history in the prologue, every fourth with an
/// editor open in the tail.
fn terminals(count: u64) -> Vec<Setup> {
    (1..=count)
        .map(|key| {
            let history = shell_output(key, 0, 40);
            let tail = if key % 4 == 0 { editor_output(key) } else { shell_output(key, 40, 8) };
            Setup { history, tail }
        })
        .collect()
}

struct Distribution {
    n: usize,
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
}

impl Distribution {
    fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort();
        let at = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
        Self { n: samples.len(), p50: at(0.50), p95: at(0.95), p99: at(0.99), max: at(1.0) }
    }
}

impl std::fmt::Display for Distribution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { n, p50, p95, p99, max } = self;
        write!(f, "n={n} p50={p50:?} p95={p95:?} p99={p99:?} max={max:?}")
    }
}

#[test]
fn a_sigkilled_worker_restores_its_16_terminals_with_equal_digests_within_100_ms() {
    let mut fixture = Fixture::start(1, terminals(16));
    let first = fixture.started(0);
    fixture.restored(0, 16);
    for key in fixture.keys() {
        let digest = fixture.full_digest(key);
        fixture.assert_record(key, first, &digest);
    }
    // Output after the seed reaches the worker as advances.
    for key in fixture.keys() {
        let end = fixture.append(key, &shell_output(key.0, 100, 3));
        fixture.wait_parsed(key, end);
    }
    let before: Vec<_> = fixture.keys().map(|key| fixture.record(key)).collect();
    for (key, record) in fixture.keys().zip(&before) {
        assert_eq!(record.digest, fixture.full_digest(key), "{key:?} before the kill");
    }

    let killed = Instant::now();
    fixture.kill(first);
    // Output while the worker is replaced.
    let moved: Vec<_> = (1..=4).map(VtTerminalKey).collect();
    for &key in &moved {
        fixture.append(key, &shell_output(key.0, 200, 2));
    }
    let lost = fixture.lost(0);
    assert!(
        matches!(lost, WorkerLifecycle::Lost { strikes: 1, next: ReseedMode::Full, .. }),
        "{lost:?}"
    );
    let second = fixture.started(0);
    let (mode, _) = fixture.restored(0, 16);
    let elapsed = killed.elapsed();
    assert_eq!(mode, ReseedMode::Full);
    assert!(elapsed < RESTORE_BOUND, "16 terminals restored {elapsed:?} after the SIGKILL");
    assert_ne!(first, second);
    for &key in &moved {
        fixture.wait_parsed(key, fixture.end(key));
    }
    for (key, before) in fixture.keys().zip(&before) {
        let digest = fixture.full_digest(key);
        fixture.assert_record(key, second, &digest);
        if !moved.contains(&key) {
            assert_eq!(digest, before.digest, "{key:?}: the restored screen is the killed one");
        }
    }
    assert!(!alive(first), "the killed worker was reaped");
    drop(fixture);
    assert!(!alive(second), "dropping the broker ends its worker");
}

#[test]
fn other_shards_echo_within_5_ms_p99_while_a_worker_restarts() {
    let mut fixture = Fixture::start(2, terminals(32));
    fixture.restored(0, 16);
    fixture.restored(1, 16);
    let victim = fixture.shard_of(VtTerminalKey(1));
    let other = 1 - victim;
    let probes: Vec<_> = fixture.keys().filter(|&key| fixture.shard_of(key) == other).collect();
    assert_eq!(probes.len(), 16);
    let bystander = fixture.worker_pid(other);

    let idle: Vec<_> = (0..100).map(|i| fixture.echo(probes[i % probes.len()])).collect();
    let killed = fixture.worker_pid(victim);
    fixture.kill(killed);
    let mut during = Vec::new();
    let mut after = 0;
    let mut restored = false;
    let deadline = Instant::now() + PATIENCE;
    while after < 50 {
        assert!(Instant::now() < deadline, "shard {victim} not restored within {PATIENCE:?}");
        let latency = fixture.echo(probes[(during.len() + after) % probes.len()]);
        if restored {
            after += 1;
        } else {
            during.push(latency);
        }
        restored |= fixture.drain(|event| {
            matches!(event, WorkerLifecycle::Restored { shard, terminals: 16, .. } if *shard == victim)
        });
    }
    let idle = Distribution::of(idle);
    let restart = Distribution::of(during.clone());
    eprintln!("other shard's echo, idle: {idle}; during the restart: {restart}");
    assert!(fixture.lost_any(victim), "the victim's worker was replaced");
    assert!(!fixture.lost_any(other), "the other shard lost its worker: {:#?}", fixture.log);
    assert_eq!(fixture.worker_pid(other), bystander);
    let replaced = fixture.worker_pid(victim);
    assert_ne!(replaced, killed);
    for key in fixture.keys().filter(|&key| !probes.contains(&key)) {
        let digest = fixture.full_digest(key);
        fixture.assert_record(key, replaced, &digest);
    }
    assert!(restart.p99 <= ECHO_P99_BOUND, "echo during the restart: {restart}");
}

#[test]
fn a_poison_pill_crashing_twice_in_10_s_restores_from_the_prologue_only() {
    // The poison is in the ring tail after the prologue: every full seed
    // replays it and crashes.
    let history = shell_output(1, 0, 10);
    let tail = [shell_output(1, 10, 2), crash_marker("abort")].concat();
    let mut fixture = Fixture::start(1, vec![Setup { history, tail }]);
    for (strikes, next) in [(1, ReseedMode::Full), (2, ReseedMode::PrologueOnly)] {
        let lost = fixture.lost(0);
        assert!(
            matches!(&lost, WorkerLifecycle::Lost { strikes: s, next: n, .. } if *s == strikes && *n == next),
            "{lost:?}"
        );
    }
    let (mode, _) = fixture.restored(0, 1);
    assert_eq!(mode, ReseedMode::PrologueOnly);
    let third = fixture.worker_pid(0);
    let digest = fixture.prologue_digest(VtTerminalKey(1));
    fixture.assert_record(VtTerminalKey(1), third, &digest);
    // The third worker lives: no further loss.
    assert_eq!(
        fixture.events.recv_timeout(Duration::from_millis(200)),
        Err(RecvTimeoutError::Timeout),
        "the prologue-only worker crashed too"
    );
    assert!(alive(third));
}

/// Ghostty's fuzz stream corpus (test/fuzz-libghostty/corpus/stream-cmin),
/// dealt into `rings` rings of at most [`CORPUS_RING`] bytes.
fn fuzz_corpus(rings: usize) -> Vec<Vec<u8>> {
    let source =
        std::env::var_os("CMUX_GHOSTTY_SRC").filter(|source| !source.is_empty()).map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../ghostty-next"),
            PathBuf::from,
        );
    let dir = source.join("test/fuzz-libghostty/corpus/stream-cmin");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("ghostty's stream corpus at {}: {error}", dir.display()))
        .map(|entry| entry.expect("a corpus entry").path())
        .collect();
    entries.sort();
    let mut out = vec![Vec::new(); rings];
    for (index, path) in entries.iter().enumerate() {
        let bytes = std::fs::read(path).expect("a corpus file");
        let ring = &mut out[index % rings];
        if ring.len() + bytes.len() <= CORPUS_RING
            && !bytes.windows(CRASH_MARK.len()).any(|window| window == CRASH_MARK)
        {
            ring.extend_from_slice(&bytes);
        }
    }
    assert!(out.iter().all(|ring| !ring.is_empty()), "an empty corpus at {}", dir.display());
    out
}

#[test]
fn each_entry_of_the_crash_corpus_kills_only_its_own_shard() {
    let corpus = fuzz_corpus(7);
    for kind in CRASH_CORPUS {
        // Terminal 1 carries the crash; 2-8 ghostty's fuzz corpus.
        let setups = std::iter::once(Setup::plain(shell_output(1, 0, 5)))
            .chain(corpus.iter().cloned().map(Setup::plain))
            .collect();
        let mut fixture = Fixture::start(2, setups);
        fixture.restored(0, 4);
        fixture.restored(1, 4);
        for key in fixture.keys() {
            let digest = fixture.full_digest(key);
            let pid = fixture.record(key).pid;
            fixture.assert_record(key, pid, &digest);
        }
        let victim = fixture.shard_of(VtTerminalKey(1));
        let other = 1 - victim;
        let bystander = fixture.worker_pid(other);

        fixture.append(VtTerminalKey(1), &crash_marker(kind));
        let lost = fixture.lost(victim);
        let expected_cause = match kind {
            "garbage-frame" => {
                matches!(lost, WorkerLifecycle::Lost { cause: LinkEnd::BadFrame(_), .. })
            }
            "cut-frame" => matches!(
                lost,
                WorkerLifecycle::Lost { cause: LinkEnd::BadFrame(_) | LinkEnd::Io(_), .. }
            ),
            _ => matches!(
                lost,
                WorkerLifecycle::Lost { cause: LinkEnd::Closed | LinkEnd::Io(_), .. }
            ),
        };
        assert!(expected_cause, "{kind}: {lost:?}");
        // The full reseed replays the crash once more; then the prologue only.
        fixture.lost(victim);
        let (mode, _) = fixture.restored(victim, 4);
        assert_eq!(mode, ReseedMode::PrologueOnly, "{kind}");
        let replaced = fixture.worker_pid(victim);
        let digest = fixture.prologue_digest(VtTerminalKey(1));
        fixture.assert_record(VtTerminalKey(1), replaced, &digest);

        // The other shard kept its worker, its screens, and its echo.
        fixture.drain(|_| false);
        assert!(!fixture.lost_any(other), "{kind} ended the other shard: {:#?}", fixture.log);
        assert_eq!(fixture.worker_pid(other), bystander, "{kind}");
        assert!(alive(bystander), "{kind}");
        let probe =
            fixture.keys().find(|&key| fixture.shard_of(key) == other).expect("a bystander");
        fixture.echo(probe);
        for key in fixture.keys().filter(|&key| fixture.shard_of(key) == other) {
            let digest = fixture.full_digest(key);
            fixture.assert_record(key, bystander, &digest);
        }
    }
}
