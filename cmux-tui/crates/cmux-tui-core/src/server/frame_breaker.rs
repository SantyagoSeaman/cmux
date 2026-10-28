//! The crash-loop breaker of framed IPC (p05-ipc-191; plans/cmux-next/
//! chrome-parity/p05-ipc.md 3.7).
//!
//! A bad message closes its connection (line_connection.rs). When the
//! connection had negotiated frames, the close also counts here: the third
//! such close within [`TRIP_WINDOW`] turns frames off for the rest of this
//! daemon's life, its epoch. `Frames` and every capability that requires it
//! are unbound (`IpcTransports::unbind_frames`, capabilities.rs), so
//! `identify` stops offering them, `set-client-info` stops granting them, and every client
//! goes on with JSON lines. A client that keeps sending a frame the daemon
//! refuses would otherwise reconnect and send it again forever. Chrome ends a
//! renderer that sends a bad message once and shows a sad tab, with no
//! reconnect loop (content/browser/bad_message.cc, `ReceivedBadMessage`);
//! here the peer reconnects, so the breaker ends the loop instead.
//!
//! Counting is lock-free: the last [`TRIP_CLOSES`] close times are atomics,
//! as Chrome's histograms record (base/metrics/histogram_macros.h).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::client_registry::ClientRegistry;

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
    tripped: AtomicBool,
}

impl FrameBreaker {
    pub(super) fn new(started: Instant) -> Self {
        FrameBreaker {
            started,
            closes: std::array::from_fn(|_| AtomicU64::new(0)),
            next: AtomicUsize::new(0),
            tripped: AtomicBool::new(false),
        }
    }

    /// Counts one close at `at`. It trips when it and the [`TRIP_CLOSES`]
    /// minus one closes before it all lie within [`TRIP_WINDOW`] of it.
    pub(super) fn record(&self, at: Instant) -> BreakerVerdict {
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
        match (full, self.tripped.swap(full, Ordering::AcqRel)) {
            (false, _) => BreakerVerdict::Counted,
            (true, false) => BreakerVerdict::Tripped,
            (true, true) => BreakerVerdict::AlreadyOff,
        }
    }

    /// Whether frames are off for this daemon's life.
    #[cfg(test)]
    pub(super) fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }
}

impl Default for FrameBreaker {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl ClientRegistry {
    /// Counts the bad-message close, at `at`, of a connection that had
    /// negotiated frames. The close that trips the breaker unbinds frames
    /// for this daemon's life.
    pub(super) fn framed_bad_message(&self, at: Instant) -> BreakerVerdict {
        let verdict = self.frame_breaker.record(at);
        if verdict == BreakerVerdict::Tripped {
            self.ipc_transports.unbind_frames();
        }
        verdict
    }
}

#[cfg(test)]
#[path = "frame_breaker_tests.rs"]
mod tests;
