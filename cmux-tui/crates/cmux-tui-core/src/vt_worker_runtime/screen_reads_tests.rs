use std::sync::Arc;
use std::sync::mpsc;
use std::time::Instant;

use ghostty_vt::{Cell, CellWidth};

use super::*;
use crate::vt_worker_runtime::fake_screen_source::{
    FakeScreenSource, parsed_terminal, published_screen, text_row, text_screen,
};

/// Output that exercises the plain formatter: styles, wide and combining
/// characters, blank rows in the middle and at the bottom, trailing blanks,
/// wrapped lines, cursor moves, erases, tabs and the alternate screen.
const CORPUS: &[&[u8]] = &[
    b"",
    b"hello",
    b"hello   \r\nworld\r\n",
    b"\x1b[1;31mred bold\x1b[0m plain \x1b[7minverse\x1b[0m",
    b"line one\r\n\r\n\r\nline four\r\n\r\n",
    "wide: \u{4f60}\u{597d} emoji \u{1f600} end".as_bytes(),
    "combining e\u{301} and a\u{308}".as_bytes(),
    b"0123456789012345678901234567890123456789wrapped onto the next row",
    b"\x1b[5;10Hplaced\x1b[2;3Hhere",
    b"erase me\x1b[1K\r\nkeep\x1b[K",
    b"a\tb\tc",
    b"primary\x1b[?1049h\x1b[Halternate screen",
    b"\x1b[44m   \x1b[0m",
    "\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}x\u{4f60}"
        .as_bytes(),
    b"\x1b[?25lhidden cursor",
];

/// Red without the page path: what `read-screen` returns from a worker page
/// is what the daemon's parser returns for the same output.
#[test]
fn page_text_equals_the_daemon_parsers_viewport_text() {
    for bytes in CORPUS {
        let mut terminal = parsed_terminal(40, 6, bytes);
        let expected = terminal.viewport_text().expect("viewport text");
        let screen = published_screen(&mut terminal);
        assert_eq!(
            screen_text(&screen.cells),
            expected,
            "output {:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn a_read_retries_a_publish_in_progress() {
    let screen = text_screen(&["ready"], 10);
    let source = FakeScreenSource::scripted(
        [Err(PageReadError::Busy), Err(PageReadError::Busy)],
        Ok(screen.clone()),
    );
    assert_eq!(read_worker_screen(&source), Ok(screen));
    assert_eq!(source.reads(), 3);
    assert!(source.bad_pages().is_empty());
}

#[test]
fn a_writer_busy_on_every_attempt_is_reported_busy() {
    let source = FakeScreenSource::steady(Err(PageReadError::Busy));
    assert_eq!(read_worker_screen(&source), Err(ScreenReadError::Busy));
    assert_eq!(source.reads(), PAGE_READ_ATTEMPTS as usize);
}

#[test]
fn an_unpublished_or_outgrown_page_is_not_a_bad_message() {
    let unpublished = FakeScreenSource::steady(Err(PageReadError::NotPublished));
    assert_eq!(read_worker_screen(&unpublished), Err(ScreenReadError::NotPublished));
    let small = FakeScreenSource::steady(Err(PageReadError::TooSmall { needed: 1 << 20 }));
    assert_eq!(read_worker_screen(&small), Err(ScreenReadError::Resizing));
    assert!(unpublished.bad_pages().is_empty() && small.bad_pages().is_empty());
}

#[test]
fn a_corrupt_page_ends_the_worker() {
    let source = FakeScreenSource::steady(Err(PageReadError::Corrupt("row table")));
    assert_eq!(read_worker_screen(&source), Err(ScreenReadError::BadPage("row table")));
    assert_eq!(source.bad_pages(), ["row table"]);
    assert_eq!(source.reads(), 1, "a bad page is not read again");
}

/// A screen that does not fit its own grid is a bad message even when the
/// page decoder let it through: nothing indexes it.
#[test]
fn a_screen_that_does_not_fit_its_grid_ends_the_worker() {
    let fits = text_screen(&["ab", "cd"], 4);
    let mut cases = Vec::new();
    let mut long_row = fits.clone();
    long_row.cells[0] = text_row("abcdef", 6);
    cases.push((long_row, "row length"));
    let mut missing_row = fits.clone();
    missing_row.rows = 3;
    cases.push((missing_row, "row count"));
    let mut cursor = fits.clone();
    cursor.cursor = (4, 0);
    cases.push((cursor, "cursor outside the grid"));
    let mut cursor_row = fits.clone();
    cursor_row.cursor = (0, 2);
    cases.push((cursor_row, "cursor outside the grid"));
    let mut huge = fits;
    huge.cols = MAX_GRID + 1;
    cases.push((huge, "grid size"));
    for (screen, reason) in cases {
        let source = FakeScreenSource::steady(Ok(screen));
        assert_eq!(read_worker_screen(&source), Err(ScreenReadError::BadPage(reason)));
        assert_eq!(source.bad_pages(), [reason]);
    }
}

#[test]
fn screens_are_read_only_for_attached_terminals() {
    let screens = WorkerScreens::default();
    let surface = 7;
    assert!(screens.read(surface).is_none(), "an unattached terminal is parsed in the daemon");
    let first = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["first"], 8))));
    screens.attach(surface, first);
    assert_eq!(
        screens.read(surface).map(|read| read.map(|screen| screen_text(&screen.cells))),
        Some(Ok("first".into()))
    );
    // A reseeded worker's page replaces the old one.
    let second = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["second"], 8))));
    screens.attach(surface, second);
    assert_eq!(
        screens.read(surface).map(|read| read.map(|screen| screen_text(&screen.cells))),
        Some(Ok("second".into()))
    );
    assert!(screens.read(surface + 1).is_none());
    assert!(screens.detach(surface));
    assert!(!screens.detach(surface));
    assert!(screens.read(surface).is_none());
}

fn history(start: u32, total: u32, rows: usize) -> HistoryPage {
    HistoryPage { start, total, epoch: 3, rows: vec![text_row("old", 10); rows] }
}

/// Runs one history request through `request_worker_history` and answers
/// it with `answer`; returns what the reply got and the bad-page reports.
fn answered(
    count: u16,
    answer: Result<HistoryPage, HistoryError>,
) -> (Result<HistoryPage, HistoryError>, Vec<&'static str>) {
    let fake = Arc::new(FakeScreenSource::steady(Err(PageReadError::NotPublished)));
    let source: Arc<dyn WorkerScreenSource> = fake.clone();
    let (sent, got) = mpsc::channel();
    let range = HistoryRange::From { start: 5, count };
    request_worker_history(&source, range, Box::new(move |page| sent.send(page).expect("send")));
    assert!(got.try_recv().is_err(), "the reply waits for the worker's answer");
    let pending = fake.take_history_requests();
    assert_eq!(pending.len(), 1);
    let request = pending.into_iter().next().expect("request");
    assert_eq!(request.range, range);
    (request.reply)(answer);
    (got.recv().expect("reply"), fake.bad_pages())
}

#[test]
fn a_history_answer_passes_its_rules_to_the_reply() {
    let page = history(5, 100, 2);
    assert_eq!(answered(2, Ok(page.clone())), (Ok(page), Vec::new()));
    // A start past the end answers no rows.
    let empty = history(100, 100, 0);
    assert_eq!(answered(10, Ok(empty.clone())), (Ok(empty), Vec::new()));
    assert_eq!(
        answered(2, Err(HistoryError::WorkerEnded)),
        (Err(HistoryError::WorkerEnded), Vec::new())
    );
}

#[test]
fn a_history_answer_that_breaks_a_rule_ends_the_worker() {
    let mut wide = history(5, 100, 1);
    wide.rows[0] = vec![Cell::default(); usize::from(MAX_GRID) + 1];
    let cases = [
        (history(5, 100, 3), "more history rows than asked"),
        (history(101, 100, 0), "history start past its total"),
        (history(99, 100, 2), "history rows past its total"),
        (wide, "history row length"),
    ];
    for (page, reason) in cases {
        assert_eq!(answered(2, Ok(page)), (Err(HistoryError::BadAnswer(reason)), vec![reason]));
    }
}

#[test]
fn spacer_cells_add_nothing_to_the_text() {
    let wide = Cell { text: "\u{4f60}".into(), width: CellWidth::Wide, ..Cell::default() };
    let tail = Cell { width: CellWidth::SpacerTail, ..Cell::default() };
    let head = Cell { width: CellWidth::SpacerHead, ..Cell::default() };
    let x = Cell { text: "x".into(), ..Cell::default() };
    assert_eq!(screen_text(&[vec![wide, tail, x, head]]), "\u{4f60}x");
    assert_eq!(screen_text(&[]), "");
    assert_eq!(screen_text(&[text_row("", 4), text_row("a", 4)]), "\na");
}

/// The agent read bench: `read-screen` of a full 200x50 worker page through
/// the registry, p50/p95/p99 and max over 2000 reads. It measures the
/// daemon's side only (the fake page's copy is a clone of the decoded
/// screen) and asserts no time: wall-clock numbers depend on the machine.
/// The acceptance number (read-screen p95 < 1 ms with process workers) comes
/// from perf-10x-lifecycle part=daemon on the fleet. Run with
/// `cargo test -p cmux-tui-core --lib read_screen_bench -- --ignored --nocapture`.
#[test]
#[ignore = "bench: prints read-screen latency, run on the fleet with --ignored"]
fn read_screen_bench_from_a_worker_page() {
    let line = "x".repeat(200);
    let lines = vec![line.as_str(); 50];
    let screens = WorkerScreens::default();
    screens.attach(1, Arc::new(FakeScreenSource::steady(Ok(text_screen(&lines, 200)))));
    let mut samples = (0..2000)
        .map(|_| {
            let start = Instant::now();
            let screen = screens.read(1).expect("attached").expect("screen");
            let text = screen_text(&screen.cells);
            assert_eq!(text.len(), 200 * 50 + 49);
            start.elapsed()
        })
        .collect::<Vec<_>>();
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    let (p50, p95, p99, max) = (at(0.5), at(0.95), at(0.99), samples[samples.len() - 1]);
    eprintln!(
        "read-screen from a worker page (200x50): p50 {p50:?} p95 {p95:?} p99 {p99:?} max {max:?}"
    );
}
