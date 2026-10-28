//! The broker against stand-in workers: threads that speak the wire
//! ([`ThreadLauncher`]). Real worker processes, SIGKILLed and crashing, are
//! in cmux-tui's `tests/vt_worker_restart.rs`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::wire::{Frame, FrameReader, Kind, SeedFrame, WIRE_VERSION, decode_offset};
use super::*;

/// How long a test waits for an event before it fails.
const PATIENCE: Duration = Duration::from_secs(10);
/// The acceptance bound: a killed worker's shard is restored within it.
const RESTORE_BOUND: Duration = Duration::from_millis(100);

/// How one stand-in worker behaves.
#[derive(Clone, Debug)]
struct FakeConfig {
    version: u16,
    /// Output offset whose parse crashes the worker.
    poison: Option<u64>,
    /// Answers sent for each seed and advance, before the Parsed that
    /// reports it (the wire's order): `(terminal, bytes)`, `None` for the
    /// parsed terminal.
    answers: Vec<(Option<u64>, Vec<u8>)>,
    /// A snapshot at each parsed offset, after the Parsed; `Some(n)` puts
    /// its base `n` past the offset parsed (a bad message).
    snapshots: Option<u64>,
    /// Replaying any prologue crashes the worker.
    prologue_crashes: bool,
    /// Never read the link (a hung worker).
    hung: bool,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            version: WIRE_VERSION,
            poison: None,
            answers: Vec::new(),
            snapshots: None,
            prologue_crashes: false,
            hung: false,
        }
    }
}

/// A seed a stand-in worker received.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SeenSeed {
    shard: usize,
    terminal: u64,
    base: u64,
    replay_from: u64,
    end: u64,
    skipped: bool,
    prologue: Vec<u8>,
}

/// Serves one link as a worker would, until the link ends or the poison
/// offset is parsed (`Err`: the crash).
fn serve(
    stream: &UnixStream,
    shard: usize,
    config: &FakeConfig,
    mut seen: impl FnMut(SeenSeed),
) -> io::Result<()> {
    let mut reader = FrameReader::new(stream.try_clone()?);
    let mut out = stream.try_clone()?;
    let mut parsed: HashMap<u64, u64> = HashMap::new();
    let crashes =
        |from: u64, to: u64| config.poison.is_some_and(|poison| (from..to).contains(&poison));
    while let Some(frame) = reader.read_frame()? {
        match frame.kind {
            Kind::Hello => out.write_all(&Frame::ready(config.version).encode())?,
            Kind::Seed => {
                let seed = SeedFrame::decode(&frame)?;
                seen(SeenSeed {
                    shard,
                    terminal: frame.terminal,
                    base: seed.base,
                    replay_from: seed.replay_from,
                    end: seed.end,
                    skipped: seed.skipped,
                    prologue: seed.prologue.to_vec(),
                });
                if crashes(seed.replay_from, seed.end)
                    || (config.prologue_crashes && !seed.prologue.is_empty())
                {
                    return Err(io::Error::other("poison"));
                }
                parsed.insert(frame.terminal, seed.end);
                report(&mut out, config, frame.terminal, seed.end)?;
            }
            Kind::Advance => {
                let end = decode_offset(&frame)?;
                let from = parsed.get(&frame.terminal).copied().unwrap_or(0);
                if crashes(from, end) {
                    return Err(io::Error::other("poison"));
                }
                parsed.insert(frame.terminal, end);
                report(&mut out, config, frame.terminal, end)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// What a worker sends for a parse that reached `end`: its answers, the
/// Parsed, then a snapshot at the parsed offset.
fn report(out: &mut UnixStream, config: &FakeConfig, terminal: u64, end: u64) -> io::Result<()> {
    for (to, bytes) in &config.answers {
        out.write_all(&Frame::answer(to.unwrap_or(terminal), bytes).encode())?;
    }
    out.write_all(&Frame::parsed(terminal, end).encode())?;
    if let Some(ahead) = config.snapshots {
        let base = end + ahead;
        out.write_all(
            &Frame::snapshot(terminal, base, format!("screen@{base}").as_bytes()).encode(),
        )?;
    }
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
/// for every later launch) and sends the test a handle to kill it.
struct ThreadLauncher {
    configs: Vec<FakeConfig>,
    launches: usize,
    seeds: Sender<SeenSeed>,
    kills: Sender<(usize, UnixStream)>,
}

impl WorkerLauncher for ThreadLauncher {
    fn launch(&mut self, shard: usize) -> io::Result<LaunchedWorker> {
        let config = self.configs[self.launches.min(self.configs.len() - 1)].clone();
        self.launches += 1;
        let (ours, theirs) = UnixStream::pair()?;
        let stream = theirs.try_clone()?;
        let _ = self.kills.send((shard, theirs.try_clone()?));
        let seeds = self.seeds.clone();
        let (stop, stopped) = channel::<()>();
        let thread = std::thread::spawn(move || {
            if config.hung {
                // Holds the link open and reads nothing until killed.
                let _ = stopped.recv();
                return;
            }
            let _ = serve(&theirs, shard, &config, |seed| {
                let _ = seeds.send(seed);
            });
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
    Snapshot(VtTerminalKey, u64, Vec<u8>),
    Life(WorkerLifecycle),
}

struct TestHost {
    reseeds: HashMap<VtTerminalKey, Reseed>,
    events: Sender<HostEvent>,
}

impl VtWorkerHost for TestHost {
    fn reseed(&mut self, terminal: VtTerminalKey) -> Option<Reseed> {
        self.reseeds.get(&terminal).cloned()
    }

    fn write_answer(&mut self, terminal: VtTerminalKey, answer: &[u8]) {
        let _ = self.events.send(HostEvent::Answer(terminal, answer.to_vec()));
    }

    fn snapshot(&mut self, terminal: VtTerminalKey, base: u64, replay: Vec<u8>) {
        let _ = self.events.send(HostEvent::Snapshot(terminal, base, replay));
    }

    fn parsed(&mut self, terminal: VtTerminalKey, offset: u64) {
        let _ = self.events.send(HostEvent::Parsed(terminal, offset));
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
    answered_spec(key, 0)
}

/// A terminal a previous daemon parsed and answered up to `answered`.
fn answered_spec(key: u64, answered: u64) -> VtTerminalSpec {
    let ring = OwnedFd::from(File::open("/dev/null").expect("open /dev/null"));
    VtTerminalSpec { key: VtTerminalKey(key), cols: 80, rows: 24, ring, answered }
}

struct Harness {
    broker: VtWorkerBroker,
    events: Receiver<HostEvent>,
    seeds: Receiver<SeenSeed>,
    kills: Receiver<(usize, UnixStream)>,
}

fn harness(
    shards: usize,
    configs: Vec<FakeConfig>,
    reseeds: HashMap<VtTerminalKey, Reseed>,
) -> Harness {
    let (events_tx, events) = channel();
    let (seeds_tx, seeds) = channel();
    let (kills_tx, kills) = channel();
    let launcher = ThreadLauncher { configs, launches: 0, seeds: seeds_tx, kills: kills_tx };
    let host = TestHost { reseeds, events: events_tx };
    let config = BrokerConfig { shards, queue: 64 };
    let broker =
        VtWorkerBroker::start(config, Box::new(launcher), Box::new(host), Box::new(Instant::now))
            .expect("broker");
    Harness { broker, events, seeds, kills }
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

fn restored(events: &Receiver<HostEvent>, shard: usize) -> (ReseedMode, usize, Duration) {
    match wait_for(
        events,
        |event| matches!(event, HostEvent::Life(WorkerLifecycle::Restored { shard: s, .. }) if *s == shard),
    ) {
        HostEvent::Life(WorkerLifecycle::Restored { mode, terminals, elapsed, .. }) => {
            (mode, terminals, elapsed)
        }
        _ => unreachable!(),
    }
}

/// Waits for the restore that ends with all `count` attached terminals
/// parsed: the first worker may report a restore of the terminals that
/// attached before its first answer, then one per later attach.
fn all_attached(events: &Receiver<HostEvent>, count: usize) {
    wait_for(
        events,
        |event| matches!(event, HostEvent::Life(WorkerLifecycle::Restored { terminals, .. }) if *terminals == count),
    );
}

#[test]
fn a_terminal_is_seeded_from_its_prologue_and_ring_tail_and_its_advances_are_relayed() {
    let h = harness(1, vec![FakeConfig::default()], reseeds([1], reseed(100, 40, 400)));
    h.broker.attach(spec(1));
    let seed = wait_for(&h.seeds, |_| true);
    assert_eq!((seed.base, seed.replay_from, seed.end, seed.skipped), (100, 100, 400, false));
    assert_eq!(seed.prologue, b"\x1b[H\x1b[2Jscreen@100");
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
    let lost =
        wait_for(&h.events, |event| matches!(event, HostEvent::Life(WorkerLifecycle::Lost { .. })));
    assert!(
        matches!(
            &lost,
            HostEvent::Life(WorkerLifecycle::Lost {
                cause: LinkEnd::Closed,
                strikes: 1,
                next: ReseedMode::Full,
                ..
            })
        ),
        "{lost:?}"
    );
    let (mode, terminals, _) = restored(&h.events, 0);
    let elapsed = killed.elapsed();
    assert_eq!((mode, terminals), (ReseedMode::Full, 16));
    assert!(elapsed < RESTORE_BOUND, "restored after {elapsed:?}");
    let reseeded: Vec<_> = h.seeds.try_iter().filter(|seed| seed.replay_from == 100).collect();
    assert_eq!(
        reseeded.len(),
        32,
        "16 first seeds and 16 reseeds, each from the prologue: {reseeded:#?}"
    );
}

#[test]
fn a_poison_pill_crashing_twice_in_10_s_restores_from_the_prologue_only() {
    let poisoned = FakeConfig { poison: Some(300), ..FakeConfig::default() };
    let h = harness(1, vec![poisoned], reseeds([1], reseed(100, 0, 400)));
    h.broker.attach(spec(1));
    let lost = |next: ReseedMode, strikes: usize| {
        let event = wait_for(&h.events, |event| {
            matches!(event, HostEvent::Life(WorkerLifecycle::Lost { .. }))
        });
        assert!(
            matches!(&event, HostEvent::Life(WorkerLifecycle::Lost { next: n, strikes: s, .. }) if *n == next && *s == strikes),
            "{event:?}"
        );
    };
    lost(ReseedMode::Full, 1);
    lost(ReseedMode::PrologueOnly, 2);
    // The prologue-only worker confirms the skipped tail, so the holder trims it.
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(1), 400));
    let (mode, terminals, _) = restored(&h.events, 0);
    assert_eq!((mode, terminals), (ReseedMode::PrologueOnly, 1));
    let seeds: Vec<_> =
        h.seeds.try_iter().map(|seed| (seed.replay_from, seed.end, seed.skipped)).collect();
    assert_eq!(seeds, [(100, 400, false), (100, 400, false), (400, 400, true)]);
}

#[test]
fn a_prologue_that_crashes_too_restores_a_blank_screen() {
    let poisoned = FakeConfig { prologue_crashes: true, ..FakeConfig::default() };
    let h = harness(1, vec![poisoned], reseeds([1], reseed(100, 0, 400)));
    h.broker.attach(spec(1));
    let (mode, terminals, _) = restored(&h.events, 0);
    assert_eq!((mode, terminals), (ReseedMode::Blank, 1));
    let seeds: Vec<_> = h
        .seeds
        .try_iter()
        .map(|seed| (seed.base, seed.replay_from, seed.prologue.is_empty()))
        .collect();
    assert_eq!(seeds, [(100, 100, false), (100, 100, false), (100, 400, false), (400, 400, true)]);
}

#[test]
fn only_answers_of_the_fixed_set_for_the_shards_own_terminals_reach_the_pty() {
    let answers = vec![
        (None, b"\x1b[?62;22c".to_vec()),
        (None, b"rm -rf ~\r".to_vec()),
        (None, b"\x1b[30;1R".to_vec()),
        (Some(999), b"\x1b[0n".to_vec()),
        (None, b"\x1b[24;80R".to_vec()),
    ];
    let h = harness(
        1,
        vec![FakeConfig { answers, ..FakeConfig::default() }],
        reseeds([1], reseed(0, 0, 0)),
    );
    h.broker.attach(spec(1));
    let mut written = Vec::new();
    let mut rejected = Vec::new();
    while written.len() + rejected.len() < 5 {
        match wait_for(&h.events, |event| {
            !matches!(
                event,
                HostEvent::Parsed(..)
                    | HostEvent::Life(
                        WorkerLifecycle::Started { .. } | WorkerLifecycle::Restored { .. }
                    )
            )
        }) {
            HostEvent::Answer(key, bytes) => written.push((key, bytes)),
            HostEvent::Life(WorkerLifecycle::AnswerRejected { terminal, reason, .. }) => {
                rejected.push((terminal, reason));
            }
            other => panic!("{other:?}"),
        }
    }
    let one = VtTerminalKey(1);
    assert_eq!(written, [(one, b"\x1b[?62;22c".to_vec()), (one, b"\x1b[24;80R".to_vec())]);
    assert_eq!(
        rejected,
        [
            (one, AnswerRejected::NotAnAnswer),
            (one, AnswerRejected::CursorOutside { row: 30, col: 1 }),
            (VtTerminalKey(999), AnswerRejected::NotInShard),
        ]
    );
}

#[test]
fn a_worker_of_another_wire_version_is_refused_and_not_restarted() {
    let h = harness(
        1,
        vec![FakeConfig { version: WIRE_VERSION + 1, ..FakeConfig::default() }],
        reseeds([1], reseed(0, 0, 0)),
    );
    h.broker.attach(spec(1));
    let refused = wait_for(&h.events, |event| {
        matches!(event, HostEvent::Life(WorkerLifecycle::Refused { .. }))
    });
    assert_eq!(
        refused,
        HostEvent::Life(WorkerLifecycle::Refused {
            shard: 0,
            generation: 1,
            version: WIRE_VERSION + 1
        })
    );
    h.broker.attach(spec(2));
    drop(h.broker);
    let launched = h
        .events
        .try_iter()
        .filter(|event| matches!(event, HostEvent::Life(WorkerLifecycle::Started { .. })))
        .count();
    assert_eq!(launched, 0, "no launch after the refusal");
}

#[test]
fn a_worker_that_stops_reading_is_replaced() {
    // 48 prologues of 128 KiB overflow the socket and a 64-frame queue.
    let mut plan = reseed(0, 0, 0);
    plan.prologue.bytes = vec![b'x'; 128 * 1024];
    let configs = vec![FakeConfig { hung: true, ..FakeConfig::default() }, FakeConfig::default()];
    let h = harness(1, configs, reseeds(1..=48, plan));
    for key in 1..=48 {
        h.broker.attach(spec(key));
    }
    let lost =
        wait_for(&h.events, |event| matches!(event, HostEvent::Life(WorkerLifecycle::Lost { .. })));
    assert!(
        matches!(lost, HostEvent::Life(WorkerLifecycle::Lost { cause: LinkEnd::Hung, .. })),
        "{lost:?}"
    );
    // Attaches race the replacement: the restore that ends with all 48 on
    // the new worker may follow one that ended with fewer.
    all_attached(&h.events, 48);
}

#[test]
fn terminals_spread_over_shards_and_a_loss_restores_only_its_own_shard() {
    let h = harness(2, vec![FakeConfig::default()], reseeds(1..=4, reseed(0, 0, 10)));
    for key in 1..=4 {
        h.broker.attach(spec(key));
    }
    let mut shards: Vec<_> = (0..4)
        .map(|_| wait_for(&h.seeds, |_| true))
        .map(|seed| (seed.terminal, seed.shard))
        .collect();
    shards.sort_unstable();
    assert_eq!(shards, [(1, 0), (2, 1), (3, 0), (4, 1)]);
    let kills: Vec<_> = (0..2).map(|_| wait_for(&h.kills, |_| true)).collect();
    for _ in 0..2 {
        wait_for(&h.events, |event| {
            matches!(event, HostEvent::Life(WorkerLifecycle::Restored { terminals: 2, .. }))
        });
    }
    let (_, shard0) = kills.iter().find(|(shard, _)| *shard == 0).expect("shard 0's worker");
    shard0.shutdown(Shutdown::Both).expect("kill shard 0's worker");
    assert_eq!(restored(&h.events, 0).1, 2);
    h.broker.advance(VtTerminalKey(2), 20);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(2), 20));
    let reseeded: Vec<_> = h.seeds.try_iter().map(|seed| seed.shard).collect();
    assert_eq!(reseeded, [0, 0], "only shard 0 reseeds");
}

const DA1: &[u8] = b"\x1b[?62;22c";

/// Answers written and answers dropped as replayed, in order, until
/// `wanted` answers in all came.
fn answers(events: &Receiver<HostEvent>, wanted: usize) -> Vec<Result<(), AnswerRejected>> {
    (0..wanted)
        .map(|_| {
            match wait_for(events, |event| {
                matches!(
                    event,
                    HostEvent::Answer(..) | HostEvent::Life(WorkerLifecycle::AnswerRejected { .. })
                )
            }) {
                HostEvent::Life(WorkerLifecycle::AnswerRejected { reason, .. }) => Err(reason),
                _ => Ok(()),
            }
        })
        .collect()
}

/// p01-process-210: a new worker replays the ring its dead predecessor
/// parsed; the queries in it were answered then, and a second answer would
/// reach the program as typed input. Output after it is answered.
#[test]
fn a_successors_replay_answers_nothing_and_new_output_is_answered() {
    let config = FakeConfig { answers: vec![(None, DA1.to_vec())], ..FakeConfig::default() };
    let h = harness(1, vec![config], reseeds([1], reseed(0, 0, 400)));
    h.broker.attach(spec(1));
    let (_, first) = wait_for(&h.kills, |_| true);
    assert_eq!(answers(&h.events, 1), [Ok(())], "the first parse answers");
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(1), 400));
    first.shutdown(Shutdown::Both).expect("kill the worker");
    assert_eq!(answers(&h.events, 1), [Err(AnswerRejected::Replayed)]);
    restored(&h.events, 0);
    h.broker.advance(VtTerminalKey(1), 500);
    assert_eq!(answers(&h.events, 1), [Ok(())], "output past the replay is answered");
}

/// p01-process-210: after a daemon restart the seed stops where the old
/// daemon's answered output ends, so no parse spans replay and new output;
/// an advance takes the worker on to the ring's end.
#[test]
fn a_restarted_daemons_seed_stops_where_the_answered_output_ends() {
    let config = FakeConfig { answers: vec![(None, DA1.to_vec())], ..FakeConfig::default() };
    let h = harness(1, vec![config], reseeds([1], reseed(100, 0, 400)));
    h.broker.attach(answered_spec(1, 250));
    let seed = wait_for(&h.seeds, |_| true);
    assert_eq!((seed.replay_from, seed.end), (100, 250));
    assert_eq!(answers(&h.events, 2), [Err(AnswerRejected::Replayed), Ok(())]);
    wait_for(&h.events, |event| *event == HostEvent::Parsed(VtTerminalKey(1), 400));
    assert_eq!(restored(&h.events, 0).1, 1);
}

/// p01-process-210: the worker's snapshots reach the holder once it
/// reported their offset parsed, newest only; one past what it parsed
/// ends the worker as a bad message.
#[test]
fn snapshots_reach_the_holder_only_at_a_parsed_offset() {
    let config = FakeConfig { snapshots: Some(0), ..FakeConfig::default() };
    let h = harness(1, vec![config], reseeds([1], reseed(0, 0, 400)));
    h.broker.attach(spec(1));
    let snapshot = |base: u64| {
        HostEvent::Snapshot(VtTerminalKey(1), base, format!("screen@{base}").into_bytes())
    };
    wait_for(&h.events, |event| *event == snapshot(400));
    h.broker.advance(VtTerminalKey(1), 500);
    wait_for(&h.events, |event| *event == snapshot(500));

    let ahead = FakeConfig { snapshots: Some(1), ..FakeConfig::default() };
    let h = harness(1, vec![ahead, FakeConfig::default()], reseeds([1], reseed(0, 0, 400)));
    h.broker.attach(spec(1));
    let lost =
        wait_for(&h.events, |event| matches!(event, HostEvent::Life(WorkerLifecycle::Lost { .. })));
    assert!(
        matches!(&lost, HostEvent::Life(WorkerLifecycle::Lost { cause: LinkEnd::BadFrame(_), .. })),
        "{lost:?}"
    );
    assert!(h.events.try_iter().all(|event| !matches!(event, HostEvent::Snapshot(..))));
}

#[test]
fn only_the_word_process_asks_for_worker_processes() {
    assert!(process_workers_requested(Some("process")));
    assert!(process_workers_requested(Some(" process\n")));
    for value in [None, Some(""), Some("threads"), Some("PROCESS"), Some("1")] {
        assert!(!process_workers_requested(value), "{value:?}");
    }
}
