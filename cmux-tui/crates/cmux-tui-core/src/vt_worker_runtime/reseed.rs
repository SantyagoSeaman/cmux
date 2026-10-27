//! Where a worker starts a terminal: the holder's prologue plus the ring
//! tail after it, cut by the shard's [`ReseedMode`].
//!
//! The holder keeps each terminal's newest prologue: a VT replay of the
//! parsed screen at output offset `base` (wire SNAPSHOT, task 210 has the
//! shard owner send it). Replaying it and then the ring from `base` gives
//! the screen at the ring's end, as a restarted daemon restores today
//! (`holder_runtime`, ATTACH with the prologue).

use super::strikes::ReseedMode;
use super::wire::SeedFrame;

/// A VT replay of a terminal's screen at output offset `base`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prologue {
    pub base: u64,
    pub bytes: Vec<u8>,
}

/// The output a terminal's ring holds: `[oldest, end)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingSpan {
    pub oldest: u64,
    pub end: u64,
}

/// What the holder side knows of a terminal when its worker (re)starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reseed {
    pub prologue: Prologue,
    pub ring: RingSpan,
}

/// The seed a worker gets: replay `prologue` (the screen at `base`), then
/// the ring from `replay_from` to `end`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedPlan {
    pub base: u64,
    pub prologue: Vec<u8>,
    pub replay_from: u64,
    pub end: u64,
    /// Output between `base` and `replay_from` is not replayed.
    pub skipped: bool,
}

impl SeedPlan {
    /// The plan for `reseed` in `mode`. Offsets are clamped so that
    /// `base <= replay_from <= end` whatever the holder side reported.
    pub fn new(reseed: Reseed, mode: ReseedMode) -> Self {
        let Reseed { prologue, ring } = reseed;
        let end = ring.end.max(ring.oldest);
        let base = prologue.base.min(end);
        match mode {
            ReseedMode::Full => {
                let replay_from = ring.oldest.clamp(base, end);
                Self {
                    base,
                    prologue: prologue.bytes,
                    replay_from,
                    end,
                    skipped: replay_from > base,
                }
            }
            ReseedMode::PrologueOnly => {
                Self { base, prologue: prologue.bytes, replay_from: end, end, skipped: end > base }
            }
            ReseedMode::Blank => {
                Self { base: end, prologue: Vec::new(), replay_from: end, end, skipped: true }
            }
        }
    }

    /// The [`super::wire::Kind::Seed`] fields.
    pub fn frame(&self) -> SeedFrame<'_> {
        SeedFrame {
            base: self.base,
            replay_from: self.replay_from,
            end: self.end,
            skipped: self.skipped,
            prologue: &self.prologue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reseed(base: u64, oldest: u64, end: u64) -> Reseed {
        Reseed {
            prologue: Prologue { base, bytes: b"\x1b[2J".to_vec() },
            ring: RingSpan { oldest, end },
        }
    }

    #[test]
    fn full_replays_the_ring_from_the_prologue() {
        let plan = SeedPlan::new(reseed(100, 50, 400), ReseedMode::Full);
        assert_eq!((plan.base, plan.replay_from, plan.end, plan.skipped), (100, 100, 400, false));
        assert_eq!(plan.prologue, b"\x1b[2J");
    }

    #[test]
    fn full_after_the_ring_rolled_past_the_prologue_marks_the_skip() {
        let plan = SeedPlan::new(reseed(100, 250, 400), ReseedMode::Full);
        assert_eq!((plan.base, plan.replay_from, plan.end, plan.skipped), (100, 250, 400, true));
    }

    #[test]
    fn prologue_only_skips_the_tail() {
        let plan = SeedPlan::new(reseed(100, 50, 400), ReseedMode::PrologueOnly);
        assert_eq!((plan.base, plan.replay_from, plan.end, plan.skipped), (100, 400, 400, true));
        assert_eq!(plan.prologue, b"\x1b[2J");
        let caught_up = SeedPlan::new(reseed(400, 50, 400), ReseedMode::PrologueOnly);
        assert!(!caught_up.skipped);
    }

    #[test]
    fn blank_keeps_nothing_old() {
        let plan = SeedPlan::new(reseed(100, 50, 400), ReseedMode::Blank);
        assert_eq!((plan.base, plan.replay_from, plan.end, plan.skipped), (400, 400, 400, true));
        assert!(plan.prologue.is_empty());
    }

    #[test]
    fn offsets_from_the_holder_side_are_clamped() {
        // A prologue past the ring's end, and a ring whose end is before its oldest.
        let plan = SeedPlan::new(reseed(900, 300, 200), ReseedMode::Full);
        assert!(plan.base <= plan.replay_from && plan.replay_from <= plan.end, "{plan:?}");
        assert_eq!(plan.end, 300);
    }
}
