//! A scripted [`WorkerScreenSource`] for tests: page reads answer from a
//! script, history requests wait until the test answers them, and bad-page
//! reports are recorded.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use cmux_threads::sync::Mutex;
use ghostty_vt::{Callbacks, Cell, RenderState, Terminal};

use super::{HistoryRange, HistoryReply, PageReadError, WorkerScreen, WorkerScreenSource};
use crate::panic_free::RecoverPoison;

/// One history request the test has not answered yet.
pub(crate) struct PendingHistory {
    pub(crate) range: HistoryRange,
    pub(crate) reply: HistoryReply,
}

pub(crate) struct FakeScreenSource {
    /// Answers of the next reads, in order; then `steady` for ever.
    script: Mutex<VecDeque<Result<WorkerScreen, PageReadError>>>,
    steady: Result<WorkerScreen, PageReadError>,
    reads: AtomicUsize,
    bad: Mutex<Vec<&'static str>>,
    history: Mutex<Vec<PendingHistory>>,
}

impl FakeScreenSource {
    pub(crate) fn steady(screen: Result<WorkerScreen, PageReadError>) -> Self {
        Self::scripted([], screen)
    }

    pub(crate) fn scripted(
        script: impl IntoIterator<Item = Result<WorkerScreen, PageReadError>>,
        steady: Result<WorkerScreen, PageReadError>,
    ) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            steady,
            reads: AtomicUsize::new(0),
            bad: Mutex::new(Vec::new()),
            history: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    pub(crate) fn bad_pages(&self) -> Vec<&'static str> {
        self.bad.lock().recover_poison().clone()
    }

    pub(crate) fn take_history_requests(&self) -> Vec<PendingHistory> {
        std::mem::take(&mut *self.history.lock().recover_poison())
    }
}

impl WorkerScreenSource for FakeScreenSource {
    fn read_page(&self) -> Result<WorkerScreen, PageReadError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.script.lock().recover_poison().pop_front().unwrap_or_else(|| self.steady.clone())
    }

    fn request_history(&self, range: HistoryRange, reply: HistoryReply) {
        self.history.lock().recover_poison().push(PendingHistory { range, reply });
    }

    fn bad_page(&self, reason: &'static str) {
        self.bad.lock().recover_poison().push(reason);
    }
}

/// The screen a worker would publish for `terminal`: its viewport cells as
/// a render walk reads them (what `cmux_vt_worker::shard` encodes).
pub(crate) fn published_screen(terminal: &mut Terminal) -> WorkerScreen {
    let mut render = RenderState::new().expect("render state");
    render.update(terminal).expect("render update");
    let mut cells = Vec::new();
    render.walk_rows(|_, _, row| cells.push(row.to_vec())).expect("walk rows");
    let cursor = render.cursor().map(|cursor| (cursor.x, cursor.y));
    WorkerScreen {
        parsed: 0,
        frame: 1,
        cols: terminal.cols(),
        rows: terminal.rows(),
        cursor,
        cursor_visible: terminal.mode(25, false),
        cells,
    }
}

/// A `cols` x `rows` terminal that parsed `bytes`.
pub(crate) fn parsed_terminal(cols: u16, rows: u16, bytes: &[u8]) -> Terminal {
    let mut terminal = Terminal::new(cols, rows, 1000, Callbacks::default()).expect("terminal");
    terminal.vt_write(bytes);
    terminal
}

/// One row of narrow cells holding `text`, padded with empty cells to `cols`.
pub(crate) fn text_row(text: &str, cols: u16) -> Vec<Cell> {
    let mut row: Vec<Cell> =
        text.chars().map(|ch| Cell { text: ch.to_string(), ..Cell::default() }).collect();
    row.resize(usize::from(cols), Cell::default());
    row
}

/// A screen of `lines` (top first), `cols` wide, with no cursor.
pub(crate) fn text_screen(lines: &[&str], cols: u16) -> WorkerScreen {
    WorkerScreen {
        parsed: 0,
        frame: 1,
        cols,
        rows: u16::try_from(lines.len()).expect("rows"),
        cursor: None,
        cursor_visible: true,
        cells: lines.iter().map(|line| text_row(line, cols)).collect(),
    }
}
