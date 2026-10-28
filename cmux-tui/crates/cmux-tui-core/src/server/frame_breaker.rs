//! The crash-loop breaker of framed IPC (p05-ipc-191; plans/cmux-next/
//! chrome-parity/p05-ipc.md 3.7).
//!
//! A bad message closes its connection (line_connection.rs). When the
//! connection had negotiated frames and its bytes broke the protocol, the
//! close also counts here; a peer that closed inside a frame is a lost peer,
//! not a protocol violation, and does not count. The third such close within
//! [`TRIP_WINDOW`] turns frames off for the rest of this daemon's life, its
//! epoch: `Frames` and every capability that requires it are withdrawn from
//! new grants (`IpcTransports::withdraw_frames`, capabilities.rs), so
//! `identify` stops offering them and `set-client-info` stops granting them.
//! New connections go on with JSON lines; a connection that already holds
//! the grant keeps it. A client that keeps sending a frame the daemon refuses
//! would otherwise reconnect and send it again forever. Chrome ends a
//! renderer that sends a bad message once and shows a sad tab, with no
//! reconnect loop (content/browser/bad_message.cc, `ReceivedBadMessage`);
//! here the peer reconnects, so the breaker ends the loop instead.
//!
//! Counting is lock-free: the last [`TRIP_CLOSES`] close times are atomics,
//! as Chrome's histograms record (base/metrics/histogram_macros.h). The
//! state shows in `server-stats` `ipc.frame_breaker`.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::client_registry::ClientRegistry;
use crate::diagnostics::IpcFrameBreakerSnapshot;

/// Closes of framed connections that trip the breaker.
pub(super) const TRIP_CLOSES: usize = 3;
/// The window those closes fall in.
pub(super) const TRIP_WINDOW: Duration = Duration::from_secs(60);

/// What one counted close did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BreakerVerdict {
    /// Counted; frames stay on.
    Counted,
    /// This close turned frames off for the daemon's life.
    Tripped,
    /// Frames were off already.
    AlreadyOff,
}

/// The last [`TRIP_CLOSES`] bad-message closes of framed connections.
pub(super) struct FrameBreaker {
    /// The origin of the stamps.
    started: Instant,
    /// Each slot: milliseconds since `started`, plus 1; 0 is empty.
    closes: [AtomicU64; TRIP_CLOSES],
    next: AtomicUsize,
    /// Every close counted, the ones after the trip included.
    counted: AtomicU64,
    tripped: AtomicBool,
}

impl FrameBreaker {
    pub(super) fn new(started: Instant) -> Self {
        FrameBreaker {
            started,
            closes: std::array::from_fn(|_| AtomicU64::new(0)),
            next: AtomicUsize::new(0),
            counted: AtomicU64::new(0),
            tripped: AtomicBool::new(false),
        }
    }

    /// Counts one close at `at`. It trips when it and the [`TRIP_CLOSES`]
    /// minus one closes before it all lie within [`TRIP_WINDOW`] of it.
    pub(super) fn record(&self, at: Instant) -> BreakerVerdict {
        self.counted.fetch_add(1, Ordering::Relaxed);
        if self.tripped.load(Ordering::Acquire) {
            return BreakerVerdict::AlreadyOff;
        }
        let since = at.saturating_duration_since(self.started).as_millis();
        let stamp = u64::try_from(since).unwrap_or(u64::MAX - 1) + 1;
        let slot = self.next.fetch_add(1, Ordering::AcqRel) % TRIP_CLOSES;
        self.closes[slot].store(stamp, Ordering::Release);
        let window = u64::try_from(TRIP_WINDOW.as_millis()).unwrap_or(u64::MAX);
        let full = self.closes.iter().all(|close| {
            let close = close.load(Ordering::Acquire);
            close != 0 && close.abs_diff(stamp) <= window
        });
        if !full {
            return BreakerVerdict::Counted;
        }
        // Only a close that fills the window writes the flag, and only to
        // set it: a concurrent close that does not trip never clears it.
        if self.tripped.swap(true, Ordering::AcqRel) {
            BreakerVerdict::AlreadyOff
        } else {
            BreakerVerdict::Tripped
        }
    }

    /// Whether frames are off for this daemon's life.
    pub(super) fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    /// The breaker's state for `server-stats` `ipc.frame_breaker`.
    pub(super) fn snapshot(&self) -> IpcFrameBreakerSnapshot {
        IpcFrameBreakerSnapshot {
            tripped: self.is_tripped(),
            framed_bad_message: self.counted.load(Ordering::Relaxed),
        }
    }
}

impl Default for FrameBreaker {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl ClientRegistry {
    /// Counts the bad-message close, at `at`, of a connection that had
    /// negotiated frames. The close that trips the breaker withdraws frames
    /// from new grants for this daemon's life.
    pub(super) fn framed_bad_message(&self, at: Instant) -> BreakerVerdict {
        let verdict = self.frame_breaker.record(at);
        if verdict == BreakerVerdict::Tripped {
            self.ipc_transports.withdraw_frames();
        }
        verdict
    }
}

#[cfg(test)]
#[path = "frame_breaker_tests.rs"]
mod tests;
