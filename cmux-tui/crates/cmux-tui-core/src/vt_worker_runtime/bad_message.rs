//! Worker output is untrusted: the `bad_message` rule (p01-process-141).
//!
//! [`WorkerOutputGuard`] mirrors what the broker told one worker (the
//! terminals attached to it, each ring end it announced, each grid it set)
//! and checks every event and page read against it. An event is good only
//! when the worker could have produced it from those commands: a terminal
//! of its shard, offsets that move forward inside the ring the daemon
//! announced, a newer frame, text without control characters, PTY answers
//! from the fixed answer set ([`super::pty_answers`]) and a page need no
//! larger than the biggest grid the terminal had. Everything else is a
//! [`BadMessage`]: the broker kills the worker and reseeds its terminals
//! from the holder prologue and ring tail ([`WorkerEnd::reseed`]).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use cmux_vt_worker::sandbox::SandboxMechanism;
use cmux_vt_worker::screen_page::{self, PageError, PageReader, Screen};
use cmux_vt_worker::wire::{self, Event, Frame, FrameReader, Geometry, WIRE_VERSION, WireError};

use super::pty_answers::{AnswerError, AnswerScanner};

/// The reason a worker ended for its output, as `debug.vt`, the strike rule
/// and the process census name it.
pub const BAD_MESSAGE_REASON: &str = "bad_message";

/// One rule a worker's output broke.
#[derive(Debug, PartialEq, Eq)]
pub enum BadMessage {
    /// The frame did not decode (kind, length, descriptors, UTF-8).
    Wire(WireError),
    /// An event before HELLO, or a second HELLO.
    HelloOrder,
    /// HELLO named another wire version.
    Version(u16),
    /// HELLO named another sandbox than the daemon gave the worker.
    Sandbox { expected: SandboxMechanism, got: SandboxMechanism },
    /// HELLO named another process than the daemon started.
    Pid { expected: u32, got: u32 },
    /// An event for a terminal the daemon never attached to this worker.
    NotInShard(u64),
    /// A parsed offset behind the last one or past the announced ring.
    ParsedOffset { id: u64, offset: u64, floor: u64, ceiling: u64 },
    /// A frame number no newer than the last one.
    Frame { id: u64, frame: u64, last: u64 },
    /// A lap that does not move forward or claims bytes the ring kept.
    Lapped { id: u64, from: u64, oldest: u64 },
    /// PTY bytes outside the answer set.
    PtyAnswer { id: u64, error: AnswerError },
    /// A title or working directory with a control character, or empty.
    Text { id: u64, what: &'static str },
    /// A bell event with no bell.
    Bell(u64),
    /// A page need larger than any grid the terminal had takes.
    PageNeed { id: u64, needed: u64, bound: u64 },
    /// A screen page that broke its layout, or a screen past the ring.
    Page { id: u64, what: &'static str },
    /// A Kitty media request while the terminal's last one is unanswered.
    MediaRequest(u64),
}

impl BadMessage {
    /// [`BAD_MESSAGE_REASON`], for every rule.
    pub fn reason(&self) -> &'static str {
        BAD_MESSAGE_REASON
    }
}

impl std::fmt::Display for BadMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(error) => write!(f, "wire: {error}"),
            Self::HelloOrder => f.write_str("HELLO out of order"),
            Self::Version(version) => write!(f, "wire version {version}"),
            Self::Sandbox { expected, got } => write!(f, "sandbox {got:?}, expected {expected:?}"),
            Self::Pid { expected, got } => write!(f, "HELLO pid {got}, started {expected}"),
            Self::NotInShard(id) => write!(f, "terminal {id} is not in the shard"),
            Self::ParsedOffset { id, offset, floor, ceiling } => {
                write!(f, "terminal {id} parsed {offset} outside {floor}..={ceiling}")
            }
            Self::Frame { id, frame, last } => {
                write!(f, "terminal {id} frame {frame} after {last}")
            }
            Self::Lapped { id, from, oldest } => {
                write!(f, "terminal {id} lapped from {from} to {oldest}")
            }
            Self::PtyAnswer { id, error } => write!(f, "terminal {id}: {error}"),
            Self::Text { id, what } => write!(f, "terminal {id}: bad {what}"),
            Self::Bell(id) => write!(f, "terminal {id}: empty bell"),
            Self::PageNeed { id, needed, bound } => {
                write!(f, "terminal {id} page need {needed} over {bound}")
            }
            Self::Page { id, what } => write!(f, "terminal {id} page: {what}"),
            Self::MediaRequest(id) => write!(f, "terminal {id}: a second open media request"),
        }
    }
}

impl std::error::Error for BadMessage {}

/// What the worker's HELLO must say: the sandbox the daemon sent and, when
/// the broker knows it, the pid it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloExpectation {
    pub sandbox: SandboxMechanism,
    pub pid: Option<u32>,
}

/// The ring offsets the daemon reads from the holder's own ring headers
/// (trusted: the holder writes them, the worker maps rings read-only).
pub trait RingOffsets {
    /// The oldest offset terminal `id`'s ring keeps now, if the daemon maps it.
    fn oldest(&self, id: u64) -> Option<u64>;
}

/// A page read the daemon may act on.
#[derive(Debug, PartialEq, Eq)]
pub enum PageRead {
    Screen(Screen),
    /// The worker has not published the page yet.
    NotPublished,
    /// The writer was busy on every attempt: read again later.
    Busy,
    /// The screen needs a page of `needed` bytes (checked like
    /// PAGE_TOO_SMALL).
    TooSmall {
        needed: u64,
    },
}

/// One terminal as the daemon told the worker about it.
#[derive(Debug)]
struct TerminalLedger {
    /// No parsed offset may go below this: the start, then the last one.
    floor: u64,
    /// The highest ring end announced, or a lap's oldest past it.
    ceiling: u64,
    frame: u64,
    /// The largest grid the terminal had on this worker.
    peak_cols: u16,
    peak_rows: u16,
    answers: AnswerScanner,
    /// A KITTY_MEDIA_REQUEST the broker has not answered yet.
    media_pending: bool,
}

impl TerminalLedger {
    fn grow(&mut self, geometry: Geometry) {
        self.peak_cols = self.peak_cols.max(geometry.cols);
        self.peak_rows = self.peak_rows.max(geometry.rows);
    }

    fn page_bound(&self) -> u64 {
        screen_page::max_image_len(self.peak_cols, self.peak_rows)
    }
}

/// The daemon's check of one worker's output.
#[derive(Debug)]
pub struct WorkerOutputGuard {
    hello: HelloExpectation,
    greeted: bool,
    terminals: BTreeMap<u64, TerminalLedger>,
    /// Terminals detached from this worker: events the worker sent before
    /// it read the DETACH are dropped, not bad. Terminal ids are never
    /// reused, and the set lives only as long as this worker.
    retired: BTreeSet<u64>,
}

impl WorkerOutputGuard {
    pub fn new(hello: HelloExpectation) -> Self {
        Self { hello, greeted: false, terminals: BTreeMap::new(), retired: BTreeSet::new() }
    }

    /// The broker sent ATTACH: `id` parses from `start` on a `geometry` grid.
    pub fn attached(&mut self, id: u64, start: u64, geometry: Geometry) {
        self.retired.remove(&id);
        let mut ledger = TerminalLedger {
            floor: start,
            ceiling: start,
            frame: 0,
            peak_cols: 0,
            peak_rows: 0,
            answers: AnswerScanner::default(),
            media_pending: false,
        };
        ledger.grow(geometry);
        self.terminals.insert(id, ledger);
    }

    /// The broker sent ADVANCE for `id` up to `end`.
    pub fn advanced(&mut self, id: u64, end: u64) {
        if let Some(ledger) = self.terminals.get_mut(&id) {
            ledger.ceiling = ledger.ceiling.max(end);
        }
    }

    /// The broker sent RESIZE for `id`.
    pub fn resized(&mut self, id: u64, geometry: Geometry) {
        if let Some(ledger) = self.terminals.get_mut(&id) {
            ledger.grow(geometry);
        }
    }

    /// The broker sent DETACH for `id`.
    pub fn detached(&mut self, id: u64) {
        if self.terminals.remove(&id).is_some() {
            self.retired.insert(id);
        }
    }

    /// The terminals attached to this worker, in id order: what the broker
    /// reseeds when the worker ends.
    pub fn terminals(&self) -> impl Iterator<Item = u64> + '_ {
        self.terminals.keys().copied()
    }

    /// Decode and check one frame. `Ok(None)` is an event with nothing to
    /// act on (a retired terminal's, or PTY bytes inside an unfinished
    /// answer).
    pub fn accept(
        &mut self,
        frame: Frame,
        rings: &dyn RingOffsets,
    ) -> Result<Option<Event>, BadMessage> {
        let event = wire::decode_event(frame).map_err(BadMessage::Wire)?;
        self.check(event, rings)
    }

    /// Check one decoded event.
    pub fn check(
        &mut self,
        event: Event,
        rings: &dyn RingOffsets,
    ) -> Result<Option<Event>, BadMessage> {
        if let Event::Hello { version, sandbox, pid } = event {
            return self.hello(version, sandbox, pid).map(|()| Some(event));
        }
        if !self.greeted {
            return Err(BadMessage::HelloOrder);
        }
        let id = event_terminal(&event);
        let Some(ledger) = self.terminals.get_mut(&id) else {
            return if self.retired.contains(&id) {
                Ok(None)
            } else {
                Err(BadMessage::NotInShard(id))
            };
        };
        match event {
            Event::Hello { .. } => Err(BadMessage::HelloOrder),
            Event::Parsed { offset, frame, .. } => {
                if offset < ledger.floor || offset > ledger.ceiling {
                    return Err(BadMessage::ParsedOffset {
                        id,
                        offset,
                        floor: ledger.floor,
                        ceiling: ledger.ceiling,
                    });
                }
                if frame <= ledger.frame {
                    return Err(BadMessage::Frame { id, frame, last: ledger.frame });
                }
                ledger.floor = offset;
                ledger.frame = frame;
                Ok(Some(event))
            }
            Event::Lapped { from, oldest, .. } => {
                // The worker read `oldest` from the ring header before this
                // check reads it, and a ring's oldest only grows.
                let kept = rings.oldest(id).unwrap_or(ledger.ceiling);
                if from < ledger.floor || from >= oldest || oldest > kept.max(ledger.ceiling) {
                    return Err(BadMessage::Lapped { id, from, oldest });
                }
                ledger.floor = oldest;
                ledger.ceiling = ledger.ceiling.max(oldest);
                Ok(Some(event))
            }
            Event::PtyWrite { bytes, .. } => {
                if bytes.is_empty() {
                    return Err(BadMessage::PtyAnswer {
                        id,
                        error: AnswerError::NotAnAnswer("empty write"),
                    });
                }
                let mut answers = Vec::new();
                ledger
                    .answers
                    .feed(&bytes, &mut answers)
                    .map_err(|error| BadMessage::PtyAnswer { id, error })?;
                Ok((!answers.is_empty()).then_some(Event::PtyWrite { id, bytes: answers }))
            }
            Event::Title { ref title, .. } => {
                check_text(id, title, "title")?;
                Ok(Some(event))
            }
            Event::Pwd { ref pwd, .. } => {
                if pwd.is_empty() {
                    return Err(BadMessage::Text { id, what: "working directory" });
                }
                check_text(id, pwd, "working directory")?;
                Ok(Some(event))
            }
            Event::Bell { count, .. } => {
                if count == 0 {
                    return Err(BadMessage::Bell(id));
                }
                Ok(Some(event))
            }
            Event::PageTooSmall { needed, .. } => {
                check_need(id, ledger, needed)?;
                Ok(Some(event))
            }
            // Bounded and checked field by field on decode (wire.rs); the
            // records take them as from the daemon's own parser.
            Event::AttachFailed { .. } | Event::ProgramStatus { .. } => Ok(Some(event)),
            Event::KittyMediaRequest { .. } => {
                // The terminal's parse waits for the answer, so a worker
                // never has two requests open for one terminal.
                if ledger.media_pending {
                    return Err(BadMessage::MediaRequest(id));
                }
                ledger.media_pending = true;
                Ok(Some(event))
            }
        }
    }

    /// The broker sent KITTY_MEDIA, the answer to `id`'s open request.
    pub fn media_answered(&mut self, id: u64) {
        if let Some(ledger) = self.terminals.get_mut(&id) {
            ledger.media_pending = false;
        }
    }

    /// Read terminal `id`'s page through `reader` and check it: a page that
    /// breaks its layout, or a screen parsed past the ring the daemon
    /// announced, is a bad message.
    pub fn read_page(&self, id: u64, reader: &mut PageReader) -> Result<PageRead, BadMessage> {
        let Some(ledger) = self.terminals.get(&id) else {
            return Err(BadMessage::NotInShard(id));
        };
        match reader.read() {
            Ok(screen) if screen.parsed > ledger.ceiling => {
                Err(BadMessage::Page { id, what: "screen parsed past the ring" })
            }
            Ok(screen) => Ok(PageRead::Screen(screen)),
            Err(PageError::NotPublished) => Ok(PageRead::NotPublished),
            Err(PageError::Busy) => Ok(PageRead::Busy),
            Err(PageError::TooSmall { needed }) => {
                check_need(id, ledger, needed)?;
                Ok(PageRead::TooSmall { needed })
            }
            Err(PageError::Corrupt(what)) => Err(BadMessage::Page { id, what }),
        }
    }

    fn hello(
        &mut self,
        version: u16,
        sandbox: SandboxMechanism,
        pid: u32,
    ) -> Result<(), BadMessage> {
        if self.greeted {
            return Err(BadMessage::HelloOrder);
        }
        if version != WIRE_VERSION {
            return Err(BadMessage::Version(version));
        }
        if sandbox != self.hello.sandbox {
            return Err(BadMessage::Sandbox { expected: self.hello.sandbox, got: sandbox });
        }
        if let Some(expected) = self.hello.pid.filter(|expected| *expected != pid) {
            return Err(BadMessage::Pid { expected, got: pid });
        }
        self.greeted = true;
        Ok(())
    }
}

fn event_terminal(event: &Event) -> u64 {
    match event {
        Event::Hello { .. } => 0,
        Event::Parsed { id, .. }
        | Event::PtyWrite { id, .. }
        | Event::Title { id, .. }
        | Event::Pwd { id, .. }
        | Event::Bell { id, .. }
        | Event::Lapped { id, .. }
        | Event::PageTooSmall { id, .. }
        | Event::AttachFailed { id, .. }
        | Event::ProgramStatus { id, .. }
        | Event::KittyMediaRequest { id, .. } => *id,
    }
}

/// Titles and working directories come from OSC strings, whose parser drops
/// every C0 control (it ends the string or is ignored), so a worker's text
/// never holds one.
fn check_text(id: u64, text: &str, what: &'static str) -> Result<(), BadMessage> {
    if text.chars().any(|c| c.is_ascii_control() && c != '\u{7f}') {
        return Err(BadMessage::Text { id, what });
    }
    Ok(())
}

fn check_need(id: u64, ledger: &TerminalLedger, needed: u64) -> Result<(), BadMessage> {
    let bound = ledger.page_bound();
    if needed <= screen_page::PAGE_HEADER as u64 || needed > bound {
        return Err(BadMessage::PageNeed { id, needed, bound });
    }
    Ok(())
}

/// The process the broker started for a worker.
pub trait WorkerProcess {
    /// End it now (SIGKILL); the broker reaps it.
    fn kill(&mut self) -> io::Result<()>;
}

impl WorkerProcess for std::process::Child {
    fn kill(&mut self) -> io::Result<()> {
        std::process::Child::kill(self)
    }
}

/// A worker ended for a bad message.
#[derive(Debug, PartialEq, Eq)]
pub struct WorkerEnd {
    pub bad: BadMessage,
    /// The terminals the broker reseeds (holder prologue plus ring tail).
    pub reseed: Vec<u64>,
    /// Set when the kill failed (the process had already exited).
    pub kill_error: Option<io::ErrorKind>,
}

impl WorkerEnd {
    /// [`BAD_MESSAGE_REASON`].
    pub fn reason(&self) -> &'static str {
        self.bad.reason()
    }
}

/// Check every whole frame `reader` holds and push the events to act on to
/// `accepted`. On the first bad frame the worker is killed before anything
/// after it is looked at, and the end names the terminals to reseed.
pub fn drain_frames(
    reader: &mut FrameReader,
    guard: &mut WorkerOutputGuard,
    rings: &dyn RingOffsets,
    process: &mut dyn WorkerProcess,
    accepted: &mut Vec<Event>,
) -> Result<(), WorkerEnd> {
    loop {
        let checked = match reader.next_frame() {
            Ok(None) => return Ok(()),
            Ok(Some(frame)) => guard.accept(frame, rings),
            Err(error) => Err(BadMessage::Wire(error)),
        };
        match checked {
            Ok(Some(event)) => accepted.push(event),
            Ok(None) => {}
            Err(bad) => return Err(end_worker(guard, process, bad)),
        }
    }
}

fn end_worker(
    guard: &WorkerOutputGuard,
    process: &mut dyn WorkerProcess,
    bad: BadMessage,
) -> WorkerEnd {
    let kill_error = process.kill().err().map(|error| error.kind());
    WorkerEnd { bad, reseed: guard.terminals().collect(), kill_error }
}

#[cfg(test)]
#[path = "bad_message_tests.rs"]
mod tests;
