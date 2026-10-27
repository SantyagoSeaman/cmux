//! Durable-lane events admitted before their commit (admit then reply, F10,
//! p05-ipc-126 on p05-ipc-120's Admitted receipt; plans/cmux-next/
//! chrome-parity/p05-ipc.md 3.6).
//!
//! A `journal-frontend-event` from a client that negotiated
//! `frontend-journal-v2` replies `admitted:true` once its event is checked
//! and queued on the durable lane, in lane order; the writer commits it
//! after the reply. Chrome's profile stores do the same: the UI thread
//! changes the model and posts the write to a sequenced background runner
//! (components/prefs/json_pref_store.cc, `ImportantFileWriter` in
//! base/files/important_file_writer.cc) instead of waiting on disk.
//!
//! The event carries an Admitted completion
//! ([`JournalIngressCompletion::Admitted`]), as 120's admitted registry
//! intents do, and the writer treats it as apply-only
//! (`QueuedJournalEvent::apply_only`). What keeps the reply true:
//! - Every check the writer would refuse the event for, other than the
//!   journal's own state, runs before admission ([`FrontendJournalEvent::validate`]),
//!   so a bad event still gets an error reply.
//! - Read-your-writes: each admitted event gets an admit sequence in lane
//!   order, and its ticket resolves when the writer is done with it. A cold
//!   journal read first waits, bounded and counted (`read_barrier_waits`),
//!   until every event admitted before it resolved
//!   ([`JournalIngressSender::admitted_read_barrier`]).
//! - The writer only applies. Contention never drops the event (a deadline
//!   expiry keeps it, `contention::expire`), and an I/O error gets the
//!   bounded retry terminal output gets. Only the content error
//!   [`FrontendEventIdReused`], or the end of those retries, drops it as
//!   journal-degraded (`journal_degraded` in `server-stats`); it never fails
//!   the writer for every later journal write.
//!
//! Behind the default-off daemon feature [`ADMIT_THEN_REPLY_FEATURE`], the
//! F10 setting (schemas/features/features.d/admitThenReply.json, p05-ipc-120).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use super::completion::JournalIngressCompletion;
use super::{
    COMMIT_PENDING, FrontendJournalEvent, JournalIngressEvent, JournalIngressSender,
    JournalIngressState, JournalLane, QueuedJournalEvent,
};

/// The registry name of the F10 daemon setting.
pub(crate) const ADMIT_THEN_REPLY_FEATURE: &str = "admitThenReply";

/// The bound of one cold read's wait for admitted events, as long as the
/// registry's read barrier for admitted intents (p05-ipc-120). A read never
/// fails on it: at the bound it reads without them and counts a timeout.
const ADMITTED_READ_BARRIER_WAIT: Duration = Duration::from_secs(2);

/// The one content error the writer finds in an admitted frontend event: its
/// event id is journaled with different content. No retry can commit it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrontendEventIdReused;

impl std::fmt::Display for FrontendEventIdReused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("frontend journal event id was reused with different content")
    }
}

impl std::error::Error for FrontendEventIdReused {}

/// True when `error` is an admitted event's own content error, which the
/// writer drops at once instead of retrying.
pub(super) fn is_content_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<FrontendEventIdReused>())
}

/// The admit gate and the read barrier's positions of one journal ingress.
#[derive(Debug, Default)]
pub(super) struct AdmittedEvents {
    enabled: AtomicBool,
    /// The admit sequence of the last admitted event the lane holds. It is
    /// set under the ingress's enqueue admission, so sequences follow lane
    /// order.
    queued_through: AtomicU64,
    /// The highest admit sequence the writer is done with. The lane and the
    /// writer keep that order, so every earlier admitted event is done too.
    resolved_through: AtomicU64,
    /// Cold reads in the barrier; a resolution wakes them only when one waits.
    barrier_waiters: AtomicUsize,
}

impl AdmittedEvents {
    /// From `CMUX_ENABLE_FEATURES` and `CMUX_DISABLE_FEATURES` (disable
    /// wins); the registry default is off on every channel.
    pub(super) fn enable_from_env(&self) {
        let enabled = cmux_features::listed_in_env(ADMIT_THEN_REPLY_FEATURE);
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    fn resolved(&self, target: u64) -> bool {
        self.resolved_through.load(Ordering::SeqCst) >= target
    }
}

/// An admitted event's place in lane order. Dropping it (the writer
/// committed, degraded or failed the event) resolves that place for the
/// read barrier.
pub(super) struct AdmittedTicket {
    state: Arc<JournalIngressState>,
    /// 0 until the lane holds the event (`try_send_in_lane_order`).
    sequence: u64,
}

impl std::fmt::Debug for AdmittedTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AdmittedTicket").field("sequence", &self.sequence).finish()
    }
}

impl AdmittedTicket {
    /// The writer did not commit this admitted event: the journal lacks
    /// what its reply said (journal-degraded).
    pub(super) fn degraded(&self, error: &str) {
        self.state.stats.journal_degraded();
        eprintln!(
            "cmux-tui: journal-degraded: admitted journal event {} did not commit: {error}",
            self.sequence
        );
    }
}

impl Drop for AdmittedTicket {
    fn drop(&mut self) {
        if self.sequence == 0 {
            return;
        }
        let admitted = &self.state.admitted;
        admitted.resolved_through.fetch_max(self.sequence, Ordering::SeqCst);
        // SeqCst with the barrier's count and load: either it sees this
        // resolution, or this sees it waiting and wakes it.
        if admitted.barrier_waiters.load(Ordering::SeqCst) > 0 {
            self.state.publish_queue_space();
        }
    }
}

impl QueuedJournalEvent {
    /// True for an event whose request replied before its commit.
    pub(super) fn is_admitted(&self) -> bool {
        self.completion.as_ref().is_some_and(JournalIngressCompletion::replied_before_commit)
    }

    fn ticket_mut(&mut self) -> Option<&mut AdmittedTicket> {
        match &mut self.completion {
            Some(JournalIngressCompletion::Admitted { ticket, .. }) => Some(ticket),
            _ => None,
        }
    }
}

impl JournalIngressState {
    /// `sender.try_send(pending)` that gives an admitted event the next admit
    /// sequence. Call it under `enqueue_admission`: that orders sequences as
    /// the lane does.
    #[expect(
        clippy::result_large_err,
        reason = "the lane's own try_send result, handed back to the enqueue loop unchanged"
    )]
    pub(super) fn try_send_in_lane_order(
        &self,
        sender: &SyncSender<QueuedJournalEvent>,
        mut pending: QueuedJournalEvent,
    ) -> Result<(), TrySendError<QueuedJournalEvent>> {
        let sequence = pending.ticket_mut().map(|ticket| {
            ticket.sequence = self.admitted.queued_through.load(Ordering::SeqCst) + 1;
            ticket.sequence
        });
        let unqueued = |mut event: QueuedJournalEvent| {
            if let Some(ticket) = event.ticket_mut() {
                ticket.sequence = 0;
            }
            event
        };
        match sender.try_send(pending) {
            Ok(()) => {
                if let Some(sequence) = sequence {
                    self.admitted.queued_through.store(sequence, Ordering::SeqCst);
                }
                Ok(())
            }
            Err(TrySendError::Full(event)) => Err(TrySendError::Full(unqueued(event))),
            Err(TrySendError::Disconnected(event)) => {
                Err(TrySendError::Disconnected(unqueued(event)))
            }
        }
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

    /// Queue durable-lane `event` with an Admitted completion and return
    /// without its receipt. The only wait is the bounded enqueue when the
    /// lane is full, a `journal_wait` at the caller as in `send_durable`.
    /// With no writer (an ephemeral session) nothing is journaled, as
    /// `send_durable` does.
    #[track_caller]
    pub(crate) fn admit_durable(&self, event: JournalIngressEvent) -> anyhow::Result<()> {
        debug_assert!(event.lane() == JournalLane::Durable);
        if let Some(error) = self.state.admission_error() {
            anyhow::bail!(error);
        }
        let Some(sender) = &self.durable_sender else { return Ok(()) };
        let deadline = self.durable_deadline();
        let completion = JournalIngressCompletion::Admitted {
            ticket: AdmittedTicket { state: self.state.clone(), sequence: 0 },
            deadline,
            commit_fence: AtomicU8::new(COMMIT_PENDING),
        };
        self.enqueue_until(
            sender,
            QueuedJournalEvent { event, completion: Some(completion) },
            deadline,
        )
        .map_err(anyhow::Error::msg)?;
        self.state.stats.admitted();
        Ok(())
    }

    /// The read barrier of a cold journal read: wait, bounded, until every
    /// event admitted before this call is done (committed or degraded), so
    /// the read sees what earlier `admitted:true` replies said. One load
    /// when none is pending. Each wait is a `journal_wait` (`request_path`)
    /// at the caller and a `read_barrier_waits` count; a read never fails
    /// here, and at the bound it goes ahead and counts
    /// `read_barrier_timeouts`. The caller must not hold the registry
    /// connection, which the writer needs; it is never the writer.
    #[track_caller]
    pub(crate) fn admitted_read_barrier(&self) {
        let admitted = &self.state.admitted;
        let target = admitted.queued_through.load(Ordering::SeqCst);
        if admitted.resolved(target) || self.is_current_writer_thread() {
            return;
        }
        crate::request_path::note(crate::request_path::BlockingKind::JournalWait);
        // The writer answers at its own class while a read waits on it.
        let _wait = self.state.request_waits.enter();
        admitted.barrier_waiters.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + ADMITTED_READ_BARRIER_WAIT;
        let reached = loop {
            let epoch = self.state.queue_space_epoch();
            if admitted.resolved(target) {
                break true;
            }
            // Errs at the bound or once the writer failed: read without it.
            if self.state.wait_for_queue_space_until(epoch, deadline).is_err() {
                break admitted.resolved(target);
            }
        };
        admitted.barrier_waiters.fetch_sub(1, Ordering::SeqCst);
        self.state.stats.read_barrier_waited(!reached);
    }
}

#[cfg(test)]
#[path = "admitted_events_tests.rs"]
mod tests;
