//! A hidden view's output, held and sent at most once per interval
//! (p14-energy-119, feature `terminalHiddenOutputBatch`).
//!
//! A view hides in place (`set-size-counts {lease, counts:false}`, the app's
//! `TerminalAttachment.sendHidden`): its stream stays attached, so a terminal
//! that prints in a hidden workspace would wake the app at the output rate.
//! While hidden, the viewer's queue keeps its output (it merges into one
//! [`super::OutputRope`]) and the worker takes it only when the batch is due:
//! 1 s after the first held byte (4 s in Energy Saver), at
//! [`HIDDEN_FLUSH_BYTES`], when anything other than plain output is queued, at
//! a bell, desktop notification or title change
//! ([`super::AttachTap::flush_held`]), when the terminal ends, or when the
//! view shows again, which flushes in the same turn.
//!
//! Chrome: a hidden page's timers run at most once per second
//! (third_party/blink/renderer/platform/scheduler/main_thread/
//! page_scheduler_impl.cc, `kDefaultThrottledWakeUpInterval`), and its
//! compositor produces no frames while hidden (cc/scheduler/scheduler.cc,
//! `SetVisible(false)` stops BeginFrames); the work it skipped runs once at
//! show.

use super::{AttachFrame, AttachFrameReceiver, AttachTap, AttachTapQueue, AttachTapState};
use crate::panic_free::RecoverPoison;
use crate::wake::EnergyMode;
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// The longest a hidden view's output waits.
pub(in crate::surface) const HIDDEN_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// The same in Energy Saver.
pub(in crate::surface) const HIDDEN_SAVER_FLUSH_INTERVAL: Duration = Duration::from_secs(4);
/// Held output at which the batch is sent at once. A queue whose budget is
/// smaller sends at half of it, so holding never makes a viewer fall behind.
pub(in crate::surface) const HIDDEN_FLUSH_BYTES: usize = 256 * 1024;

/// The batch state of a hidden viewer's queue.
pub(super) struct HiddenBatch {
    energy: Arc<EnergyMode>,
    /// When the first output of this batch was queued.
    opened: Option<Instant>,
    /// The batch is due: the worker takes every queued item, then the next
    /// output opens a new batch.
    flushing: bool,
}

impl HiddenBatch {
    fn interval(&self) -> Duration {
        if self.energy.is_saver() { HIDDEN_SAVER_FLUSH_INTERVAL } else { HIDDEN_FLUSH_INTERVAL }
    }
}

impl AttachTapQueue {
    /// The queue took `plain_output` (else another item) at `now`; returns
    /// whether the worker should wake for it. The first output after a
    /// drained queue opens a batch and wakes the worker once, to wait for
    /// the batch's due time; any other item ends the batch; output added to
    /// an open batch wakes nobody.
    pub(super) fn note_queued(
        &mut self,
        plain_output: bool,
        was_empty: bool,
        now: Instant,
    ) -> bool {
        let Some(batch) = self.hidden.as_mut() else { return true };
        if was_empty {
            batch.opened = None;
            batch.flushing = false;
        }
        let opens = plain_output && batch.opened.is_none();
        if plain_output {
            batch.opened.get_or_insert(now);
        } else {
            batch.flushing = true;
        }
        self.held_until(now).is_none() || opens
    }

    /// When the held output becomes due, or `None` when the worker may take
    /// the next item now.
    pub(super) fn held_until(&mut self, now: Instant) -> Option<Instant> {
        let flush_bytes = HIDDEN_FLUSH_BYTES.min(self.max_retained_bytes / 2);
        let full =
            self.retained_bytes >= flush_bytes || self.frames.len() >= self.max_frames.div_ceil(2);
        let ended = !self.sender_alive;
        let empty = self.frames.is_empty();
        let batch = self.hidden.as_mut()?;
        if empty || batch.flushing {
            return None;
        }
        let due = batch.opened.map(|opened| opened + batch.interval());
        match due {
            Some(due) if now < due && !full && !ended => Some(due),
            _ => {
                batch.flushing = true;
                None
            }
        }
    }

    /// When the open batch's interval ends, whatever the time now: a
    /// poller that found the queue held arms its turn for it.
    fn held_due(&self) -> Option<Instant> {
        let batch = self.hidden.as_ref()?;
        if self.frames.is_empty() || batch.flushing {
            return None;
        }
        batch.opened.map(|opened| opened + batch.interval())
    }

    /// End the batch: a bell, notification or title is in the held output.
    pub(super) fn flush_held(&mut self) -> bool {
        match self.hidden.as_mut() {
            Some(batch) if !self.frames.is_empty() && !batch.flushing => {
                batch.flushing = true;
                true
            }
            _ => false,
        }
    }
}

/// Hides or shows one viewer's output; holds no strong reference to its
/// queue, so a registry entry outliving the viewer is inert.
#[derive(Clone)]
pub(crate) struct HiddenOutput {
    state: Weak<AttachTapState>,
}

impl HiddenOutput {
    pub(super) fn new(state: &Arc<AttachTapState>) -> Self {
        Self { state: Arc::downgrade(state) }
    }

    /// Hold this viewer's output from now on, on `energy`'s interval. What it
    /// queued while visible is still sent at once. Hiding a hidden viewer
    /// keeps its batch.
    pub(crate) fn hide(&self, energy: &Arc<EnergyMode>) {
        let Some(state) = self.state.upgrade() else { return };
        let mut queue = state.queue.lock().recover_poison();
        if queue.hidden.is_some() {
            return;
        }
        let flushing = !queue.frames.is_empty();
        queue.hidden = Some(HiddenBatch { energy: energy.clone(), opened: None, flushing });
    }

    /// Send what is held now and stop holding: the worker wakes in this turn.
    pub(crate) fn show(&self) {
        let Some(state) = self.state.upgrade() else { return };
        let mut queue = state.queue.lock().recover_poison();
        if queue.hidden.take().is_none() {
            return;
        }
        drop(queue);
        state.wake_all();
    }

    /// Whether this viewer's output is held (tests).
    #[cfg(test)]
    pub(crate) fn is_hidden(&self) -> bool {
        self.state
            .upgrade()
            .is_some_and(|state| state.queue.lock().recover_poison().hidden.is_some())
    }
}

impl AttachFrameReceiver {
    /// The handle that hides and shows this viewer's output.
    pub(crate) fn hidden_output(&self) -> HiddenOutput {
        HiddenOutput::new(&self.state)
    }

    /// When this hidden viewer's held output becomes due, or `None` when
    /// nothing is held; a past time when it is due already. A drain that
    /// polls (`try_recv`, `try_viewer_event`) arms a turn for it: the batch
    /// becoming due wakes nobody.
    pub(crate) fn held_until(&self) -> Option<Instant> {
        self.state.queue.lock().recover_poison().held_due()
    }

    /// The next frame as of `at`, the batch clock, waiting up to `wait` of
    /// real time for one to become takeable (an urgent flush, a show).
    #[cfg(test)]
    pub(crate) fn recv_at(
        &self,
        at: Instant,
        wait: Duration,
    ) -> Result<AttachFrame, std::sync::mpsc::RecvTimeoutError> {
        let deadline = Instant::now() + wait;
        let mut queue = self.state.queue.lock().recover_poison();
        loop {
            if let Some(frame) = Self::pop(&mut queue, at) {
                drop(queue);
                return Ok(frame.into_frame());
            }
            if !queue.sender_alive {
                return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
            }
            if Instant::now() >= deadline {
                return Err(std::sync::mpsc::RecvTimeoutError::Timeout);
            }
            queue = self.wait(queue, Some(deadline));
        }
    }

    /// [`Self::try_recv`] as of `now`, the time a hidden viewer's batch is
    /// judged due by.
    pub(crate) fn try_recv_at(&self, now: Instant) -> Result<AttachFrame, TryRecvError> {
        let mut queue = self.state.queue.lock().recover_poison();
        if let Some(frame) = Self::pop(&mut queue, now) {
            drop(queue);
            Ok(frame.into_frame())
        } else if queue.sender_alive {
            Err(TryRecvError::Empty)
        } else {
            Err(TryRecvError::Disconnected)
        }
    }
}

impl AttachTap {
    /// Send a hidden viewer's held output now: it rang a bell, asked for a
    /// desktop notification or changed the title.
    pub(in crate::surface) fn flush_held(&self) {
        let mut queue = self.state.queue.lock().recover_poison();
        if queue.flush_held() {
            drop(queue);
            self.state.wake_one();
        }
    }
}
