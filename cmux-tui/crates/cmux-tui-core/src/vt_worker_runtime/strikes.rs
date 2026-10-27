//! The strike rule: how a terminal reseeds after its workers died while
//! parsing it, and when a shard stops getting workers.
//!
//! The holder's poison-pill rule for daemons (`cmux-holder`'s
//! `Terminal::strikes`, terminal-host-model-DESIGN.md section 3) applied to
//! VT workers. A worker death counts against the terminals whose parse was
//! in flight when it died (a terminal with nothing to parse cannot have
//! killed it): the second such death within [`STRIKE_WINDOW`] reseeds that
//! terminal from its prologue only (the ring tail is skipped), the third
//! from a blank screen at the ring's end (the prologue itself may be what
//! kills it), and the fourth hands it back to the daemon: its live output
//! keeps killing the parser. The other terminals of the shard reseed in
//! full: they did nothing wrong.
//!
//! Every death and every failed launch also counts against the shard. The
//! first [`IMMEDIATE_RESTARTS`] within the window restart at once (a killed
//! worker is replaced within one frame); later ones wait an exponential
//! backoff, and at [`SHARD_GIVE_UP`] the shard gets no more workers and its
//! terminals go back to the daemon for good. Chromium counts GPU process
//! crashes in a window the same way and falls back to a safer mode, then
//! for good, instead of restarting the same way again
//! (content/browser/gpu/gpu_process_host.cc, `RecordProcessCrash`;
//! `GpuDataManagerImplPrivate::FallBackToNextGpuMode`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a death counts.
pub const STRIKE_WINDOW: Duration = Duration::from_secs(10);
/// A terminal's strikes within the window that skip its ring tail.
pub const POISON_STRIKES: usize = 2;
/// A terminal's strikes within the window that hand it back to the daemon.
pub const TERMINAL_GIVE_UP: usize = 4;
/// A shard's deaths and failed launches within the window that restart at once.
pub const IMMEDIATE_RESTARTS: usize = 3;
/// The first backoff after the immediate restarts; it doubles per death.
pub const FIRST_BACKOFF: Duration = Duration::from_millis(50);
/// The longest backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(2);
/// A shard's deaths and failed launches within the window that end it.
pub const SHARD_GIVE_UP: usize = 10;

/// How a terminal is reseeded on a new worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    /// The reseed of a terminal with `strikes` recent strikes; `None`:
    /// hand it back.
    pub const fn after(strikes: usize) -> Option<Self> {
        match strikes {
            0 | 1 => Some(Self::Full),
            POISON_STRIKES => Some(Self::PrologueOnly),
            n if n < TERMINAL_GIVE_UP => Some(Self::Blank),
            _ => None,
        }
    }
}

/// What a shard does after a death or a failed launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// Launch a new worker after this long (zero: now).
    After(Duration),
    /// No more workers: hand the shard's terminals back.
    GiveUp,
}

impl Restart {
    /// The restart after a shard's `deaths` recent deaths.
    pub fn after(deaths: usize) -> Self {
        if deaths >= SHARD_GIVE_UP {
            return Self::GiveUp;
        }
        let Some(backed_off) = deaths.checked_sub(IMMEDIATE_RESTARTS + 1) else {
            return Self::After(Duration::ZERO);
        };
        let factor = 1u32 << backed_off.min(16);
        Self::After(FIRST_BACKOFF.saturating_mul(factor).min(MAX_BACKOFF))
    }
}

/// Recent deaths, of a shard or against a terminal.
#[derive(Debug, Default)]
pub struct StrikeLog {
    strikes: VecDeque<Instant>,
}

impl StrikeLog {
    /// Record a death at `now`; the deaths within the window, this one
    /// included.
    pub fn strike(&mut self, now: Instant) -> usize {
        while self
            .strikes
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= STRIKE_WINDOW)
        {
            self.strikes.pop_front();
        }
        self.strikes.push_back(now);
        self.strikes.len()
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
    fn a_terminals_second_strike_skips_the_tail_its_third_the_prologue_its_fourth_hands_it_back() {
        let start = Instant::now();
        let mut log = StrikeLog::default();
        let modes: Vec<_> = [0, 9_000, 9_500, 9_900]
            .map(|ms| ReseedMode::after(log.strike(start + Duration::from_millis(ms))))
            .into();
        assert_eq!(
            modes,
            [Some(ReseedMode::Full), Some(ReseedMode::PrologueOnly), Some(ReseedMode::Blank), None]
        );
    }

    #[test]
    fn strikes_older_than_the_window_are_forgiven() {
        let start = Instant::now();
        let mut log = StrikeLog::default();
        assert_eq!(log.strike(start), 1);
        assert_eq!(log.strike(start + STRIKE_WINDOW), 1);
        assert_eq!(log.strike(start + STRIKE_WINDOW * 3), 1);
        assert_eq!(log.recent(start + STRIKE_WINDOW * 3), 1);
    }

    #[test]
    fn a_shard_restarts_at_once_then_backs_off_then_gives_up() {
        let delays: Vec<_> = (1..=SHARD_GIVE_UP).map(Restart::after).collect();
        assert_eq!(delays[..IMMEDIATE_RESTARTS], [Restart::After(Duration::ZERO); 3]);
        assert_eq!(delays[IMMEDIATE_RESTARTS], Restart::After(FIRST_BACKOFF));
        assert_eq!(delays[IMMEDIATE_RESTARTS + 1], Restart::After(FIRST_BACKOFF * 2));
        assert_eq!(delays[SHARD_GIVE_UP - 2], Restart::After(Duration::from_millis(1_600)));
        assert_eq!(delays[SHARD_GIVE_UP - 1], Restart::GiveUp);
        assert_eq!(Restart::after(SHARD_GIVE_UP + 5), Restart::GiveUp);
    }
}
