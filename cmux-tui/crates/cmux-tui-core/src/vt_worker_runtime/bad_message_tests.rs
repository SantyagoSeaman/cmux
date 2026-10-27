use std::collections::HashMap;

use super::*;

const GRID: Geometry = Geometry { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };

#[derive(Default)]
struct Rings(HashMap<u64, u64>);

impl RingOffsets for Rings {
    fn oldest(&self, id: u64) -> Option<u64> {
        self.0.get(&id).copied()
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
    guard.attached(1, 100, GRID);
    guard.advanced(1, 200);
    guard
}

fn check(guard: &mut WorkerOutputGuard, event: Event) -> Result<Option<Event>, BadMessage> {
    guard.check(event, &Rings::default())
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
    assert_eq!(check(&mut guard, Event::Bell { id: 1, count: 1 }), Ok(None));
    assert_eq!(guard.terminals().count(), 0);
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
    rings.0.insert(1, 500);
    let mut guard = guard();
    let lapped = |from, oldest| Event::Lapped { id: 1, from, oldest };
    // Past the announced end but not past what the ring keeps now.
    assert!(guard.check(lapped(100, 450), &rings).is_ok());
    assert!(guard.check(Event::Parsed { id: 1, offset: 450, frame: 1 }, &rings).is_ok());
    assert!(matches!(guard.check(lapped(450, 600), &rings), Err(BadMessage::Lapped { .. })));
    assert!(matches!(guard.check(lapped(450, 450), &rings), Err(BadMessage::Lapped { .. })));
    assert!(matches!(guard.check(lapped(400, 480), &rings), Err(BadMessage::Lapped { .. })));
}

#[test]
fn pty_writes_release_only_whole_answers() {
    let mut guard = guard();
    let write = |bytes: &[u8]| Event::PtyWrite { id: 1, bytes: bytes.to_vec() };
    assert_eq!(check(&mut guard, write(b"\x1b[0n\x1b[2")), Ok(Some(write(b"\x1b[0n"))));
    assert_eq!(check(&mut guard, write(b"4;80R")), Ok(Some(write(b"\x1b[24;80R"))));
    assert!(matches!(check(&mut guard, write(b"ls\r")), Err(BadMessage::PtyAnswer { .. })));
    let mut other = self::guard();
    assert_eq!(
        check(&mut other, write(b"\x1b]52;c;eA==\x07")),
        Err(BadMessage::PtyAnswer { id: 1, error: AnswerError::Osc52 })
    );
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
fn every_bad_message_is_named_bad_message() {
    assert_eq!(BadMessage::HelloOrder.reason(), "bad_message");
    assert_eq!(BadMessage::Page { id: 1, what: "magic" }.reason(), BAD_MESSAGE_REASON);
}
