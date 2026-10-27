//! The strike rule of one shard: how a restarted worker reseeds after its
//! predecessors died.
//!
//! The holder's poison-pill rule for daemons (`cmux-holder`'s
//! `Terminal::strikes`, terminal-host-model-DESIGN.md section 3) applied to
//! a shard: a worker that died while parsing would reparse the same ring
//! tail after a restart and die again. A second death within
//! [`STRIKE_WINDOW`] restores the shard from its prologues only (the ring
//! tail is skipped), a third from a blank screen at the ring's end (the
//! prologue itself may be what kills it), so no byte stream loops a
//! restart. Chromium counts GPU process crashes in a window the same way and
//! falls back to a safer mode instead of restarting the same way again
//! (content/browser/gpu/gpu_process_host.cc, `RecordProcessCrash`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Deaths within [`STRIKE_WINDOW`] that make a shard skip its ring tail.
pub const POISON_STRIKES: usize = 2;
/// How long a death counts against its shard.
pub const STRIKE_WINDOW: Duration = Duration::from_secs(10);

/// How a shard's terminals are reseeded on a new worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReseedMode {
    /// The holder's prologue, then the ring from the prologue's base.
    Full,
    /// The prologue only; the ring tail is skipped.
    PrologueOnly,
    /// Nothing old: a blank screen at the ring's end.
    Blank,
}

impl ReseedMode {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::PrologueOnly => "prologue-only",
            Self::Blank => "blank",
        }
    }
}

/// The recent worker deaths of one shard.
#[derive(Debug, Default)]
pub struct StrikeLog {
    strikes: VecDeque<Instant>,
}

impl StrikeLog {
    /// Record a death at `now`; the mode the next worker reseeds with.
    pub fn strike(&mut self, now: Instant) -> ReseedMode {
        while self
            .strikes
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= STRIKE_WINDOW)
        {
            self.strikes.pop_front();
        }
        self.strikes.push_back(now);
        match self.strikes.len() {
            0 | 1 => ReseedMode::Full,
            n if n == POISON_STRIKES => ReseedMode::PrologueOnly,
            _ => ReseedMode::Blank,
        }
    }

    /// Deaths counted at `now`.
    pub fn recent(&self, now: Instant) -> usize {
        self.strikes.iter().filter(|at| now.saturating_duration_since(**at) < STRIKE_WINDOW).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_death_in_the_window_skips_the_tail_and_a_third_the_prologue() {
        let start = Instant::now();
        let mut log = StrikeLog::default();
        assert_eq!(log.strike(start), ReseedMode::Full);
        assert_eq!(log.strike(start + Duration::from_secs(9)), ReseedMode::PrologueOnly);
        assert_eq!(log.strike(start + Duration::from_millis(9_500)), ReseedMode::Blank);
        assert_eq!(log.recent(start + Duration::from_millis(9_500)), 3);
    }

    #[test]
    fn deaths_older_than_the_window_are_forgiven() {
        let start = Instant::now();
        let mut log = StrikeLog::default();
        assert_eq!(log.strike(start), ReseedMode::Full);
        assert_eq!(log.strike(start + STRIKE_WINDOW), ReseedMode::Full);
        assert_eq!(log.strike(start + STRIKE_WINDOW * 3), ReseedMode::Full);
        assert_eq!(log.recent(start + STRIKE_WINDOW * 3), 1);
    }
}
