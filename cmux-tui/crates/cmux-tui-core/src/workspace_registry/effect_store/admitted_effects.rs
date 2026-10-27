//! Effect receipts admitted before the journal writer applies them (admit
//! then reply, F10, p05-ipc-125).
//!
//! An admitted effect's receipt (its resource revision, and whether a retry
//! replays it) is decided at prepare. Until the writer answers, SQLite does
//! not hold it, so the next prepare takes the resource head and this key's
//! admitted receipt from here and reads only rows no admitted intent can
//! change, instead of waiting behind the connection's read barrier. LevelDB
//! reads its memtable before the table files the same way
//! (third_party/leveldatabase/src/db/db_impl.cc, `DBImpl::Get`).
//!
//! The head is known only while every unanswered admission of the
//! connection is one tracked here. Another site's admission, or a commit
//! that did not go through here, can move the resource head: prepare then
//! reads behind the barrier, and commits and waits when the barrier gives up
//! (`AdmittedEffectStats::waited_unsettled`).

use std::collections::HashMap;

/// Tracked admissions at most; past it prepare reads behind the barrier,
/// which settles them.
const MAX_TRACKED: u64 = 256;

/// A tracked admission's receipt: what a retry of its key replays.
#[derive(Debug, Clone)]
pub(super) struct AdmittedReceipt {
    pub(super) outcome_json: String,
    pub(super) revision: u64,
}

/// The effect admissions of one registry since the store last held every
/// admitted intent. Only the registry holder touches it.
#[derive(Debug, Default)]
pub(crate) struct AdmittedEffects {
    /// The connection's admission count when the store last held every
    /// admitted intent.
    base: u64,
    /// Admissions tracked since `base`, in admit order.
    tracked: u64,
    /// The resource revision after the newest tracked admission; `None`
    /// when a commit it did not see may have moved it.
    head: Option<u64>,
    receipts: HashMap<String, AdmittedReceipt>,
    stats: AdmittedEffectStats,
}

/// How effect prepares found their receipt.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "server-stats reads them with admit_stats (p05-ipc-120)")
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AdmittedEffectStats {
    /// Prepares that took the head from tracked admissions, past the barrier.
    pub(crate) tracked_prepares: u64,
    /// Prepares that read behind the barrier while an untracked admission
    /// was unanswered.
    pub(crate) barrier_prepares: u64,
    /// Of those, commits that waited for the writer because the barrier
    /// gave up before the store held every admitted intent.
    pub(crate) waited_unsettled: u64,
    /// Patch commits that waited for the writer: the writer's patch apply
    /// has refusals prepare does not run.
    pub(crate) waited_patch: u64,
}

impl AdmittedEffects {
    /// The store holds every admission so far (`admitted`) at resource
    /// revision `head`.
    pub(super) fn settle(&mut self, admitted: u64, head: u64) {
        self.base = admitted;
        self.tracked = 0;
        self.head = Some(head);
        self.receipts.clear();
    }

    /// The resource head after every admission so far (`admitted`), when
    /// each unanswered one is tracked here.
    pub(super) fn head(&self, admitted: u64) -> Option<u64> {
        let covers =
            self.tracked < MAX_TRACKED && self.base.saturating_add(self.tracked) == admitted;
        self.head.filter(|_| covers)
    }

    /// The receipt of `key`'s tracked admission, which SQLite may not hold yet.
    pub(super) fn receipt(&self, key: &str) -> Option<&AdmittedReceipt> {
        self.receipts.get(key)
    }

    /// `key` was admitted as admission `sequence` with `receipt`.
    pub(super) fn admitted(&mut self, key: String, sequence: u64, receipt: AdmittedReceipt) {
        let next = self.head.is_some() && sequence == self.base + self.tracked + 1;
        self.tracked += 1;
        self.head = next.then_some(receipt.revision);
        self.receipts.insert(key, receipt);
    }

    /// A commit this did not admit follows: the head is unknown until the
    /// store settles again.
    pub(super) fn forget_head(&mut self) {
        self.head = None;
    }

    pub(super) fn stats_mut(&mut self) -> &mut AdmittedEffectStats {
        &mut self.stats
    }

    #[cfg(test)]
    pub(super) fn stats(&self) -> AdmittedEffectStats {
        self.stats
    }
}
