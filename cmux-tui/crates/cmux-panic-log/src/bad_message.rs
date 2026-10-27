//! Bad messages: a peer that broke its wire's rules.
//!
//! Chrome's `mojo::ReportBadMessage` (mojo/public/cpp/bindings/message.h,
//! content/browser/bad_message.cc `ReceivedBadMessage`) ends the misbehaving
//! peer, never the browser, counts the reason
//! (`Stability.BadMessageTerminated.*`) and takes one crash dump without
//! crashing. [`BadMessages::report_bad_message`] is that report for one
//! daemon: it counts the reason and, the first time a reason is seen in the
//! ledger's life (one launch: the daemon owns one ledger), writes one record
//! to the process's bad-message log. The caller closes that peer and only
//! that peer; every other connection keeps working.
//!
//! The ledger is lock-free: each reason claims one of [`MAX_REASONS`] slots
//! once, and a report is one atomic add on its slot. Reasons past the last
//! slot count under [`OVERFLOW_REASON`].
//!
//! The log is a sink the process installs once, like the hang log
//! ([`crate::hang`]); before [`install`] a record goes nowhere.

use std::fmt::Display;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinct reasons one ledger counts by name.
pub const MAX_REASONS: usize = 64;
/// Where reasons past [`MAX_REASONS`] count.
pub const OVERFLOW_REASON: &str = "other";
/// The `kind` of a bad-message record.
pub const RECORD_KIND: &str = "bad_message";

/// Where records go: called once per reason per ledger.
pub type BadMessageSink = Box<dyn Fn(serde_json::Value) + Send + Sync>;

/// Whether this report was its reason's first in the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reported {
    /// The first of its reason: its record went to the sink.
    First,
    /// Counted only; the reason was reported before.
    Repeat,
}

struct Slot {
    reason: OnceLock<&'static str>,
    count: AtomicU64,
}

/// One daemon's bad messages by reason.
pub struct BadMessages {
    slots: [Slot; MAX_REASONS],
    overflow: AtomicU64,
    sink: Option<BadMessageSink>,
}

impl Default for BadMessages {
    /// A ledger that writes to the process's installed log ([`install`]).
    fn default() -> Self {
        Self::build(None)
    }
}

impl BadMessages {
    /// A ledger that writes each first report to `sink` instead of the
    /// process's log (tests, a host with its own log).
    pub fn with_sink(sink: BadMessageSink) -> Self {
        Self::build(Some(sink))
    }

    fn build(sink: Option<BadMessageSink>) -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot {
                reason: OnceLock::new(),
                count: AtomicU64::new(0),
            }),
            overflow: AtomicU64::new(0),
            sink,
        }
    }

    /// `peer` sent a bad message: count `reason` and, on its first report,
    /// write one record naming `peer`, `reason` and `detail`. The caller then
    /// closes `peer`'s connection, and only it. `detail` must describe the
    /// wire fault (lengths, kinds, offsets), never message content.
    pub fn report_bad_message(
        &self,
        peer: &str,
        reason: &'static str,
        detail: &dyn Display,
    ) -> Reported {
        let (reason, counter) = self.counter(reason);
        let before = counter.fetch_add(1, Ordering::Relaxed);
        if before != 0 {
            return Reported::Repeat;
        }
        let line = record(peer, reason, &detail.to_string());
        match &self.sink {
            Some(sink) => sink(line),
            None => write(line),
        }
        Reported::First
    }

    /// Reports of `reason` so far (an overflowed reason counts under
    /// [`OVERFLOW_REASON`]).
    pub fn count(&self, reason: &str) -> u64 {
        if reason == OVERFLOW_REASON {
            return self.overflow.load(Ordering::Relaxed);
        }
        self.slots
            .iter()
            .find(|slot| slot.reason.get().is_some_and(|claimed| *claimed == reason))
            .map_or(0, |slot| slot.count.load(Ordering::Relaxed))
    }

    /// Every reason reported so far with its count, in first-seen order;
    /// [`OVERFLOW_REASON`] last when any overflowed.
    pub fn counts(&self) -> Vec<(&'static str, u64)> {
        let mut counts: Vec<(&'static str, u64)> = self
            .slots
            .iter()
            .filter_map(|slot| {
                let reason = slot.reason.get()?;
                Some((*reason, slot.count.load(Ordering::Relaxed)))
            })
            .collect();
        let overflow = self.overflow.load(Ordering::Relaxed);
        if overflow > 0 {
            counts.push((OVERFLOW_REASON, overflow));
        }
        counts
    }

    /// The slot of `reason`, claimed if it has none yet.
    fn counter(&self, reason: &'static str) -> (&'static str, &AtomicU64) {
        if reason == OVERFLOW_REASON {
            return (OVERFLOW_REASON, &self.overflow);
        }
        for slot in &self.slots {
            // An empty slot is claimed by the first reporter; a racing
            // claimer of another reason sees it taken and moves on.
            let claimed = *slot.reason.get_or_init(|| reason);
            if claimed == reason {
                return (reason, &slot.count);
            }
        }
        (OVERFLOW_REASON, &self.overflow)
    }
}

/// The record of one first report: `kind`, `peer`, `reason` and a `message`
/// of both, with the panic log's `thread`, `at_ms` and `test` keys. It goes
/// to its own log, never the panic log: a peer's fault is not a crash.
pub fn record(peer: &str, reason: &str, detail: &str) -> serde_json::Value {
    let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
    let message = format!("bad_message {reason} from {peer}: {detail}");
    let mut line =
        crate::record(&crate::Fields::new(), &message, "", &thread, false, String::new());
    if let serde_json::Value::Object(map) = &mut line {
        map.insert("kind".into(), RECORD_KIND.into());
        map.insert("peer".into(), peer.into());
        map.insert("reason".into(), reason.into());
        map.remove("backtrace");
    }
    line
}

/// A sink that appends each record, with `fields`, to the JSONL at `path`
/// (a bad-message log beside the panic log, written the same way:
/// owner-only, never through a link, size-capped).
pub fn file_sink(path: PathBuf, fields: crate::Fields) -> BadMessageSink {
    Box::new(move |record| {
        let serde_json::Value::Object(record) = record else { return };
        let mut line = fields.clone();
        line.extend(record);
        crate::append(&path, &serde_json::Value::Object(line));
    })
}

static INSTALLED: OnceLock<BadMessageSink> = OnceLock::new();

/// Make `sink` the process's bad-message log. A second install is refused
/// and returns its sink.
pub fn install(sink: BadMessageSink) -> Result<(), BadMessageSink> {
    INSTALLED.set(sink)
}

/// Write `record` to the process's bad-message log; nothing before
/// [`install`].
pub fn write(record: serde_json::Value) {
    if let Some(sink) = INSTALLED.get() {
        sink(record);
    }
}

#[cfg(test)]
#[path = "bad_message_tests.rs"]
mod tests;
