//! Durable-lane events admitted without a receipt (admit then reply, F10,
//! p05-ipc-126; plans/cmux-next/chrome-parity/p05-ipc.md 3.6).
//!
//! A `journal-frontend-event` from a client that negotiated
//! `frontend-journal-v2` replies `admitted:true` once its event is checked
//! and queued on the durable lane, in lane order; the writer commits it
//! after the reply. Chrome's profile stores do the same: the UI thread
//! changes the model and posts the write to a sequenced background runner
//! (components/prefs/json_pref_store.cc, `ImportantFileWriter` in
//! base/files/important_file_writer.cc) instead of waiting on disk.
//!
//! What keeps the reply true:
//! - Every check the writer would refuse the event for, other than the
//!   journal's own state, runs before admission ([`FrontendJournalEvent::validate`]),
//!   so a bad event still gets an error reply.
//! - The writer only applies. An admitted event that fails on its own
//!   (an event id reused with different content, an I/O error) is dropped
//!   and counted as journal-degraded; it never fails the writer for every
//!   later journal write, as a terminal-lane failure does.
//! - Contention never drops it: a deadline expiry keeps events without a
//!   receipt for the next attempt (`contention::expire`).
//!
//! Behind the default-off daemon feature [`ADMIT_THEN_REPLY_FEATURE`], the
//! F10 setting (schemas/features/features.d/admitThenReply.json, p05-ipc-120).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{
    FrontendJournalEvent, JournalIngressEvent, JournalIngressSender, JournalLane,
    QueuedJournalEvent,
};

/// The registry name of the F10 daemon setting.
pub(crate) const ADMIT_THEN_REPLY_FEATURE: &str = "admitThenReply";

/// The admit gate and counters of one journal ingress.
#[derive(Debug, Default)]
pub(super) struct AdmittedEvents {
    enabled: AtomicBool,
    admitted: AtomicU64,
    degraded: AtomicU64,
}

/// [`AdmittedEvents`]' counters at one moment.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AdmittedEventStats {
    /// Events queued without a receipt wait.
    pub(crate) admitted: u64,
    /// Admitted events the writer dropped: the journal does not hold them.
    pub(crate) degraded: u64,
}

impl AdmittedEvents {
    /// From `CMUX_ENABLE_FEATURES` and `CMUX_DISABLE_FEATURES` (disable
    /// wins); the registry default is off on every channel.
    pub(super) fn enable_from_env(&self) {
        let enabled = cmux_features::listed_in_env(ADMIT_THEN_REPLY_FEATURE);
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// The writer dropped an admitted event that failed on its own.
    pub(super) fn degraded(&self, error: &str) {
        self.degraded.fetch_add(1, Ordering::Relaxed);
        eprintln!("cmux-tui: journal-degraded: an admitted journal event did not commit: {error}");
    }
}

impl QueuedJournalEvent {
    /// True for an event whose request replied before its commit: a
    /// durable-lane event queued without a receipt. Every other
    /// durable-lane event carries one.
    pub(super) fn is_admitted(&self) -> bool {
        self.completion.is_none() && self.event.lane() == JournalLane::Durable
    }
}

impl FrontendJournalEvent {
    /// The checks the writer refuses a frontend event for without reading
    /// the journal (workspace_registry/journal_extensions/writer_batch.rs).
    /// An admitted event runs them before its reply; a duplicate event id
    /// is the only content error left to the writer.
    pub(crate) fn validate(&self, principal_id: &str) -> anyhow::Result<()> {
        use crate::workspace_registry::validate_identifier;
        validate_identifier("frontend journal principal", principal_id)?;
        validate_identifier("frontend journal generation", self.generation())?;
        validate_identifier("frontend journal event id", self.event_id())?;
        if let Self::Resize { cols, rows, cell_width, cell_height, .. } = self {
            anyhow::ensure!(
                *cols > 0 && *rows > 0 && *cell_width > 0 && *cell_height > 0,
                "frontend journal geometry must be positive"
            );
        }
        Ok(())
    }
}

impl JournalIngressSender {
    /// Whether durable events may reply before their commit (the F10
    /// setting is on).
    pub(crate) fn admits_before_commit(&self) -> bool {
        self.state.admitted.enabled.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn set_admit_then_reply_for_test(&self, enabled: bool) {
        self.state.admitted.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Queue durable-lane `event` and return without its receipt. The only
    /// wait is the bounded enqueue when the lane is full, a `journal_wait`
    /// at the caller as in `send_durable`. With no writer (an ephemeral
    /// session) nothing is journaled, as `send_durable` does.
    #[track_caller]
    pub(crate) fn admit_durable(&self, event: JournalIngressEvent) -> anyhow::Result<()> {
        debug_assert!(event.lane() == JournalLane::Durable);
        if let Some(error) = self.state.admission_error() {
            anyhow::bail!(error);
        }
        let Some(sender) = &self.durable_sender else { return Ok(()) };
        let deadline = self.durable_deadline();
        self.enqueue_until(sender, QueuedJournalEvent { event, completion: None }, deadline)
            .map_err(anyhow::Error::msg)?;
        self.state.admitted.admitted.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn admitted_event_stats(&self) -> AdmittedEventStats {
        let admitted = &self.state.admitted;
        AdmittedEventStats {
            admitted: admitted.admitted.load(Ordering::Relaxed),
            degraded: admitted.degraded.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
#[path = "admitted_events_tests.rs"]
mod tests;
