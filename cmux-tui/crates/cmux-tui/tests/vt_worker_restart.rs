//! The VT worker broker (p01-process-134) with real worker processes: this
//! test binary, started again by the broker's `ProcessLauncher` as a
//! stand-in `cmux-vt-worker` that speaks the broker's wire.
//!
//! - A SIGKILLed worker is replaced and its 16 terminals are restored
//!   within 100 ms.
//! - A poison pill (output whose parse crashes the worker) that crashes it
//!   twice within 10 s is restored from the prologue only: the third worker
//!   never sees the poisoned ring tail.
//!
//! Task 136 adds the real `cmux-vt-worker`, digests and the ghostty-vt crash
//! corpus.
#![cfg(unix)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use cmux_tui_core::vt_worker_runtime::wire::{
    Frame, FrameReader, Kind, SeedFrame, WIRE_VERSION, decode_offset,
};
use cmux_tui_core::vt_worker_runtime::{
    BrokerConfig, ProcessLauncher, Prologue, Reseed, ReseedMode, RingSpan, VtTerminalKey,
    VtTerminalSpec, VtWorkerBroker, VtWorkerHost, WORKER_FD, WorkerLifecycle,
};

/// The env entry that makes this binary a stand-in worker; its value is
/// the poisoned output offset, or empty for none.
const FAKE_WORKER_ENV: &str = "CMUX_VT_FAKE_WORKER";
/// How long a test waits for an event before it fails.
const PATIENCE: Duration = Duration::from_secs(10);
/// The acceptance bound for a restore after a SIGKILL.
const RESTORE_BOUND: Duration = Duration::from_millis(100);
/// The prologue base and ring end of every terminal here.
const BASE: u64 = 100;
const END: u64 = 400;

/// Not a test: the body of the stand-in worker. Run by the harness without
/// [`FAKE_WORKER_ENV`], it does nothing.
#[test]
fn fake_worker_process() {
    let Some(poison) = std::env::var_os(FAKE_WORKER_ENV) else { return };
    let poison: Option<u64> = poison.to_str().and_then(|value| value.parse().ok());
    // SAFETY: the broker's launcher gives this process its link at WORKER_FD,
    // and nothing else here owns that descriptor.
    let stream = unsafe { UnixStream::from_raw_fd(WORKER_FD) };
    if serve(&stream, poison).is_err() {
        // What a parser crash looks like to the broker: the process ends.
        std::process::abort();
    }
    std::process::exit(0);
}

/// Answers the broker as a worker would; `Err` when it reaches `poison`.
fn serve(stream: &UnixStream, poison: Option<u64>) -> io::Result<()> {
    let mut reader = FrameReader::new(stream.try_clone()?);
    let mut out = stream.try_clone()?;
    let mut parsed: HashMap<u64, u64> = HashMap::new();
    let crashes = |from: u64, to: u64| poison.is_some_and(|poison| (from..to).contains(&poison));
    while let Some(frame) = reader.read_frame()? {
        let (from, to) = match frame.kind {
            Kind::Hello => {
                out.write_all(&Frame::ready(WIRE_VERSION).encode())?;
                continue;
            }
            Kind::Seed => {
                let seed = SeedFrame::decode(&frame)?;
                (seed.replay_from, seed.end)
            }
            Kind::Advance => {
                (parsed.get(&frame.terminal).copied().unwrap_or(0), decode_offset(&frame)?)
            }
            _ => continue,
        };
        if crashes(from, to) {
            return Err(io::Error::other("poison"));
        }
        parsed.insert(frame.terminal, to);
        out.write_all(&Frame::parsed(frame.terminal, to).encode())?;
    }
    Ok(())
}

struct Host {
    events: Sender<WorkerLifecycle>,
}

impl VtWorkerHost for Host {
    fn reseed(&mut self, _terminal: VtTerminalKey) -> Option<Reseed> {
        Some(Reseed {
            prologue: Prologue { base: BASE, bytes: b"\x1b[H\x1b[2Jprologue".to_vec() },
            ring: RingSpan { oldest: 0, end: END },
        })
    }

    fn write_answer(&mut self, _terminal: VtTerminalKey, _answer: &[u8]) {}

    fn snapshot(&mut self, _terminal: VtTerminalKey, _base: u64, _replay: Vec<u8>) {}

    fn parsed(&mut self, _terminal: VtTerminalKey, _offset: u64) {}

    fn lifecycle(&mut self, event: WorkerLifecycle) {
        let _ = self.events.send(event);
    }
}

fn start(poison: Option<u64>, terminals: u64) -> (VtWorkerBroker, Receiver<WorkerLifecycle>) {
    let exe = std::env::current_exe().expect("this test binary");
    let launcher = ProcessLauncher::new(exe)
        .arg("--exact")
        .arg("fake_worker_process")
        .arg("--test-threads=1")
        .env(FAKE_WORKER_ENV, poison.map(|offset| offset.to_string()).unwrap_or_default());
    let (events, received) = channel();
    let config = BrokerConfig { shards: 1, queue: 256 };
    let broker = VtWorkerBroker::start(
        config,
        Box::new(launcher),
        Box::new(Host { events }),
        Box::new(Instant::now),
    )
    .expect("start the broker");
    for key in 1..=terminals {
        let ring = OwnedFd::from(File::open("/dev/null").expect("open /dev/null"));
        broker.attach(VtTerminalSpec {
            key: VtTerminalKey(key),
            cols: 80,
            rows: 24,
            ring,
            answered: 0,
        });
    }
    (broker, received)
}

fn wait_for(
    events: &Receiver<WorkerLifecycle>,
    mut wanted: impl FnMut(&WorkerLifecycle) -> bool,
) -> WorkerLifecycle {
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

fn started(events: &Receiver<WorkerLifecycle>) -> u32 {
    match wait_for(events, |event| matches!(event, WorkerLifecycle::Started { .. })) {
        WorkerLifecycle::Started { pid, .. } => pid,
        _ => unreachable!(),
    }
}

/// The restore that ends with `terminals` parsed: its mode and the time
/// from the loss (or start) that needed it.
fn restored(events: &Receiver<WorkerLifecycle>, terminals: usize) -> (ReseedMode, Duration) {
    match wait_for(
        events,
        |event| matches!(event, WorkerLifecycle::Restored { terminals: n, .. } if *n == terminals),
    ) {
        WorkerLifecycle::Restored { mode, elapsed, .. } => (mode, elapsed),
        _ => unreachable!(),
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[test]
fn a_sigkilled_worker_restores_its_16_terminals_within_100_ms() {
    let (broker, events) = start(None, 16);
    let first = started(&events);
    restored(&events, 16);
    let killed = Instant::now();
    // SAFETY: kill(2) with a pid the broker started and has not reaped.
    assert_eq!(unsafe { libc::kill(first as libc::pid_t, libc::SIGKILL) }, 0);
    let lost = wait_for(&events, |event| matches!(event, WorkerLifecycle::Lost { .. }));
    assert!(
        matches!(lost, WorkerLifecycle::Lost { strikes: 1, next: ReseedMode::Full, .. }),
        "{lost:?}"
    );
    let second = started(&events);
    let (mode, _) = restored(&events, 16);
    let elapsed = killed.elapsed();
    assert_eq!(mode, ReseedMode::Full);
    assert!(elapsed < RESTORE_BOUND, "16 terminals restored {elapsed:?} after the SIGKILL");
    assert_ne!(first, second);
    assert!(!alive(first), "the killed worker was reaped");
    drop(broker);
    assert!(!alive(second), "dropping the broker ends its worker");
}

#[test]
fn a_poison_pill_crashing_twice_in_10_s_restores_from_the_prologue_only() {
    // The poison is in the ring tail after the prologue: every full seed
    // replays it and crashes.
    let (broker, events) = start(Some(300), 1);
    for (strikes, next) in [(1, ReseedMode::Full), (2, ReseedMode::PrologueOnly)] {
        let lost = wait_for(&events, |event| matches!(event, WorkerLifecycle::Lost { .. }));
        assert!(
            matches!(&lost, WorkerLifecycle::Lost { strikes: s, next: n, .. } if *s == strikes && *n == next),
            "{lost:?}"
        );
    }
    let (mode, _) = restored(&events, 1);
    assert_eq!(mode, ReseedMode::PrologueOnly);
    // The third worker lives: no further loss.
    assert!(
        events
            .recv_timeout(Duration::from_millis(200))
            .is_err_and(|error| { error == std::sync::mpsc::RecvTimeoutError::Timeout }),
        "the prologue-only worker crashed too"
    );
    drop(broker);
}
