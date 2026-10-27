//! Registry commits through the journal writer batch.
//!
//! Lock order: workspace registry -> registry connection -> state. The
//! caller holds the registry from prepare to admission (topology writers
//! stay serialized, so a projected tree cannot go stale before its
//! revision) but never the connection: the journal writer needs the
//! connection to commit, and with `admitThenReply` off the caller waits for
//! the writer's receipt.
//! The writer takes neither the registry nor state
//! (`write_path.writer_registry_locks` in `server-stats` stays 0). The wait
//! is bounded (see `journal_ingress::effect_intents`): a writer error, a
//! dropped receipt or an expired deadline returns an error, and the caller's
//! registry guard drops with it.

use std::sync::Weak;

use super::*;
use crate::journal_ingress::EffectSend;
use crate::workspace_registry::{
    EffectCommitFinish, EffectCommitIntent, EffectCommitReceipt, RegistryIntent,
    RegistryIntentSink, RegistryReceipt,
};

/// The journal writer as the registry's intent sink. It holds the mux
/// weakly: the registry connection must not keep the mux alive.
pub(crate) struct MuxIntentSink(pub(crate) Weak<Mux>);

impl RegistryIntentSink for MuxIntentSink {
    fn accepts(&self) -> bool {
        self.0.upgrade().is_some_and(|mux| mux.journal_ingress.accepts_effects())
    }

    #[track_caller]
    fn send(
        &self,
        intent: RegistryIntent,
        site: &'static std::panic::Location<'static>,
    ) -> EffectSend {
        match self.0.upgrade() {
            Some(mux) => mux.journal_ingress.send_effect(intent, site),
            None => EffectSend::NotQueued(Box::new(intent)),
        }
    }
}

impl Mux {
    /// Send registry commits to the journal writer from now on (called when
    /// the writer starts).
    pub(crate) fn install_registry_intent_sink(self: &Arc<Self>) {
        self.registry_connection.install_intent_sink(Arc::new(MuxIntentSink(Arc::downgrade(self))));
    }

    /// Commit a prepared effect intent and finish it under `registry`
    /// (`WorkspaceRegistry::admit_effect_commit`): an outcome is admitted
    /// with the receipt decided at prepare (admit then reply, F10,
    /// p05-ipc-125) and the journal writer applies it later, in admit order;
    /// a patch, an outcome prepared while the store misses an untracked
    /// admission, or any intent with `admitThenReply` off commits and waits
    /// for the writer's receipt. A replayed receipt (the store or an earlier
    /// admission already holds this outcome) writes nothing and is not
    /// finished again.
    #[track_caller]
    pub(crate) fn admit_effect_intent(
        &self,
        registry: &mut WorkspaceRegistry,
        intent: EffectCommitIntent,
        finish: EffectCommitFinish,
    ) -> anyhow::Result<EffectCommitReceipt> {
        let receipt = match registry.admit_effect_commit(intent) {
            Ok(receipt) if receipt.replayed() => return Ok(receipt),
            Ok(receipt) => receipt,
            Err(error) => {
                // An indeterminate receipt: the writer can still commit after
                // this request gives up. Wake journal subscribers, who read
                // the head. Any other error committed nothing.
                if error
                    .downcast_ref::<crate::journal_ingress::JournalCommitIndeterminate>()
                    .is_some()
                {
                    self.publish_journal_event();
                }
                return Err(error);
            }
        };
        registry.finish_effect_commit(finish, receipt)
    }

    /// Apply the receipts of earlier indeterminate registry commits that
    /// arrived since (cx-g1fa.3): each one's owed state update, then raise
    /// the in-memory revisions to what the journal committed. The caller
    /// holds the registry and `state`.
    #[track_caller]
    pub(crate) fn settle_registry_receipts(&self, registry: &WorkspaceRegistry, state: &mut State) {
        let settled = registry.connection.take_settled_receipts();
        if settled.is_empty() {
            return;
        }
        for (receipt, settle) in settled {
            settle_one(state, &receipt, settle);
        }
        // Subscribers read the new head (publishing takes neither the
        // registry nor state).
        self.publish_resource_event();
    }
}

#[track_caller]
fn settle_one(
    state: &mut State,
    receipt: &RegistryReceipt,
    settle: Option<crate::workspace_registry::SettleState>,
) {
    let (resource_revision, workspace_revision) =
        (state.resource_revision, state.workspace_revision);
    if let Some(settle) = settle {
        settle(state, receipt);
    }
    // A later commit may already have installed a higher revision.
    state.resource_revision = state
        .resource_revision
        .max(resource_revision)
        .max(receipt.resource_revision().unwrap_or(0));
    state.workspace_revision = state.workspace_revision.max(workspace_revision);
}
