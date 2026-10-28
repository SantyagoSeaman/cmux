//! A long read's hold on its surface. Later requests of the surface wait
//! for the long read (spec/events.md, Ordering Guarantees), so the hold
//! lasts until its answer is sent. Most long reads answer on their run; one
//! whose answer leaves later, from an `AsyncReply`
//! (`read-scrollback` of a terminal a VT worker parses, p01-process-135),
//! takes the hold into its reply with [`take`], and the surface stays held
//! until the worker answered.
//!
//! The hold is the running long read's, on the thread that runs it, as the
//! request's cause and lock scope are (`crate::event_cause`,
//! `lock_caller_scope`).

use std::cell::RefCell;
use std::sync::Arc;

use super::ConnectionSurfaceScheduler;
use crate::SurfaceId;

thread_local! {
    static RUNNING: RefCell<Option<LongReadHold>> = const { RefCell::new(None) };
}

/// Holds `surface` in its connection's scheduler until dropped.
pub(in crate::server) struct LongReadHold {
    scheduler: Arc<ConnectionSurfaceScheduler>,
    surface: SurfaceId,
}

impl LongReadHold {
    pub(super) fn new(scheduler: Arc<ConnectionSurfaceScheduler>, surface: SurfaceId) -> Self {
        Self { scheduler, surface }
    }
}

impl Drop for LongReadHold {
    fn drop(&mut self) {
        self.scheduler.finish_read(self.surface);
    }
}

/// Runs `run` as the long read that owns `hold`; the hold ends when `run`
/// returns or unwinds, unless the request took it ([`take`]).
pub(super) fn run_holding<R>(hold: LongReadHold, run: impl FnOnce() -> R) -> R {
    /// Puts back what ran before and drops the hold if it is still here,
    /// outside the cell's borrow (its drop wakes the scheduler).
    struct Restore(Option<LongReadHold>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            let ours = RUNNING.with(|running| running.replace(previous));
            drop(ours);
        }
    }
    let previous = RUNNING.with(|running| running.replace(Some(hold)));
    let _restore = Restore(previous);
    run()
}

/// The running long read's hold, for a request whose answer leaves after
/// its run; `None` when the request is not a long read.
pub(in crate::server) fn take() -> Option<LongReadHold> {
    RUNNING.with(|running| running.borrow_mut().take())
}
