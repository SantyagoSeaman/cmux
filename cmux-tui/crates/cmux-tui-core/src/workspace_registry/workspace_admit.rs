//! Workspace registry commits that reply at admit (F10, p05-ipc-124;
//! plans/cmux-next/chrome-parity/p05-ipc.md 3.6).
//!
//! A workspace registry revision (the ledger row, its event, with
//! `project_resource` the resource projection, and the rows the commit also
//! writes: presentation, a creation's state mark) is decided on the request
//! thread and admitted on the journal writer's durable lane
//! ([`WorkspaceRegistry::admit_registry_intent`]); the writer commits the
//! same intent later, in admit order. Chromium orders work when it is
//! scheduled and lets the backing store only apply it
//! (content/browser/indexed_db/indexed_db_transaction_coordinator.cc).
//!
//! The prepared receipt is the intent applied in a transaction that rolls
//! back. Every check of the commit and of its rows runs there (replay,
//! generation and revision CAS, a second workspace for an app, a missing
//! icon asset or group), so a refused request is an error at the request,
//! never a journal-degraded writer answer. The read barrier of the first
//! connection acquisition (or of the caller's pin) puts every earlier
//! admitted intent in SQLite first, and the caller holds the registry until
//! it admits, so the writer applies the intent to the state it was decided
//! on. The same transaction reads what the caller installs in memory after
//! the reply (the presentation), so nothing rereads SQLite after admit.
//!
//! The caller's connection pin is released only to admit: with the feature
//! off or no writer, the intent commits locally under the pin, as before.
//! No admitted path can leave an indeterminate receipt, so no state settle
//! is owed.

use super::*;

/// What a workspace registry commit's reply carries.
#[derive(Debug)]
pub(crate) struct AdmittedWorkspaceCommit {
    pub(crate) commit: RegistryCommit,
    /// The resource revision the commit installs (`project_resource`, not
    /// replayed).
    pub(crate) resource_revision: Option<u64>,
    /// The presentation with this commit's rows, when asked for and not
    /// replayed.
    pub(crate) presentation: Option<PresentationSnapshot>,
}

impl WorkspaceRegistry {
    /// Decide `intent`'s receipt and admit it (or, after a counted
    /// fallback, commit it). `release_pin` drops the caller's connection
    /// pin; it runs only when the intent goes to the writer's lane. With
    /// `read_presentation`, the reply carries the presentation the commit
    /// leaves. A replayed mutation is answered here and admits nothing.
    #[track_caller]
    pub(crate) fn admit_workspace_commit(
        &mut self,
        intent: WorkspaceCommitIntent,
        read_presentation: bool,
        release_pin: impl FnOnce(),
    ) -> anyhow::Result<AdmittedWorkspaceCommit> {
        let (prepared, presentation) = self.decide_workspace_commit(&intent, read_presentation)?;
        if prepared.commit.replayed {
            return Ok(AdmittedWorkspaceCommit {
                commit: prepared.commit,
                resource_revision: None,
                presentation: None,
            });
        }
        let admits = self.connection.admit.enabled();
        let receipt = self
            .admit_registry_intent(
                RegistryIntent::Workspace(intent),
                RegistryReceipt::Workspace(prepared),
                || {
                    if admits {
                        release_pin();
                    }
                },
                None,
            )?
            .into_receipt()
            .into_workspace()?;
        Ok(AdmittedWorkspaceCommit {
            commit: receipt.commit,
            resource_revision: receipt.resource_revision,
            presentation,
        })
    }

    /// `intent` applied in a transaction that rolls back: its receipt, and
    /// with `read_presentation` the presentation it leaves.
    #[track_caller]
    fn decide_workspace_commit(
        &self,
        intent: &WorkspaceCommitIntent,
        read_presentation: bool,
    ) -> anyhow::Result<(WorkspaceCommitReceipt, Option<PresentationSnapshot>)> {
        let db = self.connection.get();
        let tx = db.unchecked_transaction()?;
        let receipt = intent.apply(&tx, None)?;
        let presentation = (read_presentation && !receipt.commit.replayed)
            .then(|| self.presentation_snapshot())
            .transpose()?;
        tx.rollback()?;
        Ok((receipt, presentation))
    }

    /// The intent of [`Self::commit_workspace_presentation`]: a workspace
    /// registry revision with `project_resource` that also writes
    /// `workspace_key`'s presentation row.
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub(crate) fn prepare_workspace_presentation_commit(
        &self,
        mutation: &WorkspaceMutation,
        fingerprint: &Value,
        expected_generation: Option<&str>,
        expected_revision: Option<u64>,
        event_kind: &str,
        workspace_key: &str,
        workspaces: &[RegistryWorkspace],
        active_workspace: Option<&WorkspacePublicId>,
        update: &WorkspacePresentationUpdate,
        result: &Value,
    ) -> anyhow::Result<WorkspaceCommitIntent> {
        update.validate()?;
        let (key, presentation) = (workspace_key.to_string(), update.clone());
        let write: OwnedTransactionWrite = Box::new(move |tx: &Transaction<'_>| {
            presentation_store::write_workspace_presentation(tx, &key, &presentation)
        });
        Ok(self
            .prepare_workspace_commit(
                mutation,
                fingerprint,
                expected_generation,
                expected_revision,
                event_kind,
                workspace_key,
                workspaces,
                active_workspace,
                result,
                true,
            )?
            .with_extra_rows(write))
    }
}

#[cfg(test)]
#[path = "workspace_admit_tests.rs"]
mod tests;
