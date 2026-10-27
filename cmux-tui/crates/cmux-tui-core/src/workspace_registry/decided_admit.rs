//! The direct registry commits that reply at admit (F10, p05-ipc-134;
//! plans/cmux-next/chrome-parity/p05-ipc.md 3.6): `commit_terminal` and the
//! workspace rename, the last callers that sent an intent to the journal
//! writer and waited for its receipt.
//!
//! The receipt is decided on the request thread by the intent's own apply,
//! in a transaction that rolls back: the writer runs the same apply later,
//! in admit order, on the same rows. The caller holds the registry from
//! prepare until it admits, and the read barrier of its first connection
//! acquisition puts every earlier admitted intent in SQLite first, so the
//! writer's answer repeats the decided one (`admitted_reply`). A refused
//! intent (replay conflict, CAS, a tombstoned id) is an error at the
//! request, never a journal-degraded writer answer. A replayed receipt
//! (the ledger holds the mutation, or a second exit hits the exit latch)
//! writes nothing and admits nothing. Chromium orders work when it is
//! scheduled and lets the store only apply it
//! (content/browser/indexed_db/indexed_db_transaction_coordinator.cc).
//!
//! With `admitThenReply` off nothing is decided: the intent goes to the
//! writer and the caller waits for its receipt, exactly as before.

use super::admitted_reply::FallbackReason;
use super::*;

impl WorkspaceRegistry {
    /// Commit one prepared terminal transition: admitted with its decided
    /// receipt, or (feature off, counted) through the writer with a wait.
    #[track_caller]
    pub(crate) fn admit_terminal_commit(
        &mut self,
        intent: TerminalCommitIntent,
    ) -> anyhow::Result<TerminalRegistryCommit> {
        self.admit_decided_intent(RegistryIntent::Terminal(intent), || {}, None)?.into_terminal()
    }

    /// Commit one prepared workspace registry revision for a caller that
    /// holds state on a pinned connection. `release_pin` drops that pin when
    /// the intent leaves for the writer's lane. `settle` is the state update
    /// owed if the commit goes to the writer with the feature off and its
    /// receipt turns indeterminate; admitted, the caller's state is the
    /// reply, and a writer failure keeps it (journal-degraded).
    #[track_caller]
    pub(crate) fn admit_workspace_commit_settled(
        &mut self,
        intent: WorkspaceCommitIntent,
        release_pin: impl FnOnce(),
        settle: SettleState,
    ) -> anyhow::Result<WorkspaceCommitReceipt> {
        self.admit_decided_intent(RegistryIntent::Workspace(intent), release_pin, Some(settle))?
            .into_workspace()
    }

    #[track_caller]
    fn admit_decided_intent(
        &mut self,
        intent: RegistryIntent,
        release_pin: impl FnOnce(),
        settle: Option<SettleState>,
    ) -> anyhow::Result<RegistryReceipt> {
        if !self.connection.admit.enabled() {
            self.connection.admit.fell_back(FallbackReason::Disabled);
            return self.commit_registry_intent_with(intent, release_pin, settle);
        }
        let prepared = self.decide_registry_intent(&intent)?;
        if receipt_replayed(&prepared) {
            return Ok(prepared);
        }
        Ok(self.admit_registry_intent(intent, prepared, release_pin, settle)?.into_receipt())
    }

    /// `intent`'s receipt: its apply in a transaction that rolls back.
    #[track_caller]
    fn decide_registry_intent(&self, intent: &RegistryIntent) -> anyhow::Result<RegistryReceipt> {
        let db = self.connection.get();
        let tx = db.unchecked_transaction()?;
        let receipt = match intent {
            RegistryIntent::Terminal(intent) => RegistryReceipt::Terminal(intent.apply(&tx)?),
            RegistryIntent::Workspace(intent) => {
                RegistryReceipt::Workspace(intent.apply(&tx, None)?)
            }
            // Effect receipts are decided by the effect store (p05-ipc-125).
            RegistryIntent::Effect(_) => {
                anyhow::bail!("an effect intent is not decided by its apply")
            }
        };
        tx.rollback()?;
        Ok(receipt)
    }
}

fn receipt_replayed(receipt: &RegistryReceipt) -> bool {
    match receipt {
        RegistryReceipt::Terminal(commit) => commit.replayed,
        RegistryReceipt::Workspace(receipt) => receipt.commit.replayed,
        RegistryReceipt::Effect(_) => false,
    }
}

#[cfg(test)]
#[path = "decided_admit_tests.rs"]
mod tests;
