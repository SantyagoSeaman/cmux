//! Screen reads from VT worker pages, no hop (p01-process-135).
//!
//! A worker publishes each terminal's viewport, cursor and modes into a
//! seqlock page that the daemon maps read-only
//! (`cmux_vt_worker::screen_page`). A read copies the page between two equal
//! even sequence numbers and decodes the copy with every count and offset
//! checked against the copy; it never sends the worker a request. A
//! `read-screen` costs one copy of the page instead of a round trip to a
//! process that may be busy parsing. Chrome reads a renderer's shared memory
//! the same way: the browser maps a `base::ReadOnlySharedMemoryRegion`
//! (base/memory/read_only_shared_memory_region.h), validates what it reads,
//! and only what is not in shared memory goes over Mojo.
//!
//! What a worker wrote is untrusted (p01-process-141; Chrome's
//! content/browser/bad_message.h). A page that breaks the layout, or a
//! decoded screen whose rows, cells or cursor do not fit its grid, is a bad
//! message: the read fails and the terminal's source hears why
//! ([`WorkerScreenSource::bad_page`]), which ends the worker and reseeds its
//! shard.
//!
//! Scrollback is not in the page. `read-scrollback` asks the worker for the
//! rows ([`WorkerScreenSource::request_history`]) and its reply leaves from
//! the answer through the request's `AsyncReply`
//! (server/deferred_commands.rs), so no request thread waits on a worker.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use cmux_threads::sync::Mutex;
use ghostty_vt::{Cell, CellWidth};

use crate::SurfaceId;
use crate::panic_free::RecoverPoison;

/// Page reads that may find the writer inside a publish before a read
/// reports [`ScreenReadError::Busy`]. The page reader already retries a
/// publish in progress; a whole publish of a large screen is about 0.1 ms,
/// so a few more tries cover one that started during the copy.
pub const PAGE_READ_ATTEMPTS: u32 = 4;
/// Rows or columns a worker screen or history row may have (the page
/// layout's bound, `cmux_vt_worker::screen_page::MAX_GRID`).
const MAX_GRID: u16 = 4096;

/// One consistent viewport copy a worker published
/// (`cmux_vt_worker::screen_page::Screen`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerScreen {
    /// The ring offset the worker had parsed when it published this screen.
    pub parsed: u64,
    /// The worker's publish count for this terminal.
    pub frame: u64,
    pub cols: u16,
    pub rows: u16,
    /// The cursor's column and row when it is visible and in the viewport.
    pub cursor: Option<(u16, u16)>,
    /// DECTCEM (DEC 25): the program shows the cursor.
    pub cursor_visible: bool,
    /// The viewport, top row first.
    pub cells: Vec<Vec<Cell>>,
}

impl WorkerScreen {
    /// The first rule this screen breaks, if any: a screen that does not fit
    /// its own grid is a bad message, never something to index with.
    fn violation(&self) -> Option<&'static str> {
        if self.cols > MAX_GRID || self.rows > MAX_GRID {
            return Some("grid size");
        }
        if self.cells.len() != usize::from(self.rows) {
            return Some("row count");
        }
        if self.cells.iter().any(|row| row.len() > usize::from(self.cols)) {
            return Some("row length");
        }
        if self.cursor.is_some_and(|(x, y)| x >= self.cols || y >= self.rows) {
            return Some("cursor outside the grid");
        }
        None
    }
}

/// Why a page read gave no screen
/// (`cmux_vt_worker::screen_page::PageError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageReadError {
    /// The worker has not published this page yet.
    NotPublished,
    /// The writer was inside a publish on every try.
    Busy,
    /// The screen does not fit the page: the broker passes a larger one.
    TooSmall { needed: u64 },
    /// The copy broke a layout rule (a bad message).
    Corrupt(&'static str),
}

/// Why a worker terminal's screen could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenReadError {
    /// The worker has not published the terminal's screen since it was
    /// assigned or reseeded.
    NotPublished,
    /// The writer was inside a publish on every attempt.
    Busy,
    /// The screen outgrew its page; the broker is passing a larger one.
    Resizing,
    /// The worker wrote a page or a screen that breaks the layout; the
    /// worker is ended and its shard reseeded.
    BadPage(&'static str),
}

impl fmt::Display for ScreenReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPublished => f.write_str("terminal screen not published by its VT worker yet"),
            Self::Busy => f.write_str("terminal screen page busy"),
            Self::Resizing => f.write_str("terminal screen page is being resized"),
            Self::BadPage(reason) => write!(f, "bad terminal screen page: {reason}"),
        }
    }
}

impl std::error::Error for ScreenReadError {}

/// Which history rows a request asks for (the oldest retained row is 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryRange {
    /// Up to `count` rows from `start` (`read-scrollback`).
    From { start: u32, count: u16 },
    /// The `limit` rows that end at `before`, or at the newest row when it
    /// is `None` or past the end (`terminal.history.read`).
    Before { before: Option<u32>, limit: u16 },
}

impl HistoryRange {
    /// The most rows an answer may hold.
    pub fn max_rows(self) -> u16 {
        match self {
            Self::From { count, .. } => count,
            Self::Before { limit, .. } => limit,
        }
    }
}

/// Styled history rows a worker sent for one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPage {
    /// The first row sent, clamped to `total` by the worker.
    pub start: u32,
    /// History rows the terminal holds.
    pub total: u32,
    /// The history epoch (`Terminal::history_epoch`): it changes when the
    /// history is cleared or rewritten.
    pub epoch: u64,
    pub rows: Vec<Vec<Cell>>,
}

impl HistoryPage {
    /// The first rule this answer to `range` breaks.
    fn violation(&self, range: HistoryRange) -> Option<&'static str> {
        if self.start > self.total {
            return Some("history start past its total");
        }
        if self.rows.len() > usize::from(range.max_rows()) {
            return Some("more history rows than asked");
        }
        let left = u64::from(self.total - self.start);
        if self.rows.len() as u64 > left {
            return Some("history rows past its total");
        }
        if self.rows.iter().any(|row| row.len() > usize::from(MAX_GRID)) {
            return Some("history row length");
        }
        None
    }
}

/// Why a history request got no rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryError {
    /// The worker ended before it answered (the terminal is being reseeded).
    WorkerEnded,
    /// The terminal left the worker before it answered.
    Released,
    /// The worker's answer broke a rule (a bad message; the worker is ended).
    BadAnswer(&'static str),
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerEnded => f.write_str("the terminal's VT worker ended before it answered"),
            Self::Released => f.write_str("the terminal left its VT worker before it answered"),
            Self::BadAnswer(reason) => write!(f, "bad scrollback answer: {reason}"),
        }
    }
}

impl std::error::Error for HistoryError {}

/// Where a history answer goes: run once, on the thread that got it.
pub type HistoryReply = Box<dyn FnOnce(Result<HistoryPage, HistoryError>) + Send>;

/// The broker's handle on one worker terminal: its mapped page and its
/// worker link.
pub trait WorkerScreenSource: Send + Sync {
    /// One consistent copy of the page, decoded with every bound checked
    /// (`cmux_vt_worker::screen_page::PageReader::read`).
    fn read_page(&self) -> Result<WorkerScreen, PageReadError>;

    /// Ask the worker for the history rows of `range`. `reply` runs exactly
    /// once: with the rows, or with why none came. The broker runs it on its
    /// link thread, never on the caller's turn.
    fn request_history(&self, range: HistoryRange, reply: HistoryReply);

    /// The worker wrote a page, screen or answer that breaks a rule: end it
    /// with reason `bad_message` and reseed its shard (p01-process-141).
    fn bad_page(&self, reason: &'static str);
}

/// Read `source`'s screen: retried while the writer is mid-publish, checked
/// against its own grid, and reported to the source when it is bad.
pub fn read_worker_screen(
    source: &dyn WorkerScreenSource,
) -> Result<WorkerScreen, ScreenReadError> {
    for _ in 0..PAGE_READ_ATTEMPTS {
        let screen = match source.read_page() {
            Ok(screen) => screen,
            Err(PageReadError::Busy) => continue,
            Err(PageReadError::NotPublished) => return Err(ScreenReadError::NotPublished),
            Err(PageReadError::TooSmall { .. }) => return Err(ScreenReadError::Resizing),
            Err(PageReadError::Corrupt(reason)) => {
                source.bad_page(reason);
                return Err(ScreenReadError::BadPage(reason));
            }
        };
        if let Some(reason) = screen.violation() {
            source.bad_page(reason);
            return Err(ScreenReadError::BadPage(reason));
        }
        return Ok(screen);
    }
    Err(ScreenReadError::Busy)
}

/// Ask `source` for history rows; `reply` gets them only after they passed
/// the answer's rules (a bad answer ends the worker).
pub fn request_worker_history(
    source: &Arc<dyn WorkerScreenSource>,
    range: HistoryRange,
    reply: HistoryReply,
) {
    let checker = Arc::clone(source);
    source.request_history(
        range,
        Box::new(move |answer| {
            let answer = answer.and_then(|page| match page.violation(range) {
                None => Ok(page),
                Some(reason) => {
                    checker.bad_page(reason);
                    Err(HistoryError::BadAnswer(reason))
                }
            });
            reply(answer);
        }),
    );
}

/// The viewport text of `rows` as `Terminal::viewport_text` formats it
/// (Ghostty's plain formatter, unwrap off, trim on): a wide character's
/// spacer cells add nothing, an empty cell is a space, every row loses its
/// trailing blanks, and blank rows at the bottom are dropped.
pub fn screen_text(rows: &[Vec<Cell>]) -> String {
    let mut text = String::new();
    let mut pending_newlines = 0usize;
    let mut line = String::new();
    for (index, row) in rows.iter().enumerate() {
        line.clear();
        for cell in row {
            match cell.width {
                CellWidth::SpacerTail | CellWidth::SpacerHead => {}
                CellWidth::Narrow | CellWidth::Wide if cell.text.is_empty() => line.push(' '),
                CellWidth::Narrow | CellWidth::Wide => line.push_str(&cell.text),
            }
        }
        let trimmed = line.trim_end_matches(' ');
        if index > 0 {
            pending_newlines += 1;
        }
        if trimmed.is_empty() {
            continue;
        }
        for _ in 0..pending_newlines {
            text.push('\n');
        }
        pending_newlines = 0;
        text.push_str(trimmed);
    }
    text
}

/// The worker terminals whose screens the daemon reads from pages, by
/// surface. The broker attaches a terminal once its worker has published
/// the first screen after its seed, and detaches it when the terminal
/// leaves the worker; a terminal not here is parsed in the daemon.
#[derive(Default)]
pub struct WorkerScreens {
    /// A leaf lock, held only to clone, insert or remove one entry: never
    /// across a page read, a history request or a reply.
    terminals: Mutex<HashMap<SurfaceId, Arc<dyn WorkerScreenSource>>>,
}

impl WorkerScreens {
    /// Read `surface`'s screen from `source` from now on (replaces the
    /// source of an earlier worker or page).
    pub fn attach(&self, surface: SurfaceId, source: Arc<dyn WorkerScreenSource>) {
        self.terminals.lock().recover_poison().insert(surface, source);
    }

    /// Stop reading `surface` from a page. Whether it was attached.
    pub fn detach(&self, surface: SurfaceId) -> bool {
        self.terminals.lock().recover_poison().remove(&surface).is_some()
    }

    /// `surface`'s page source, when a worker parses it.
    pub fn source(&self, surface: SurfaceId) -> Option<Arc<dyn WorkerScreenSource>> {
        self.terminals.lock().recover_poison().get(&surface).cloned()
    }

    /// `surface`'s screen from its page, or `None` when the daemon parses it.
    pub fn read(&self, surface: SurfaceId) -> Option<Result<WorkerScreen, ScreenReadError>> {
        let source = self.source(surface)?;
        Some(read_worker_screen(source.as_ref()))
    }
}

#[cfg(test)]
#[path = "screen_reads_tests.rs"]
mod tests;
