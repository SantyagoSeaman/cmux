//! Workspace registry commits that reply at admit (F10, p05-ipc-124;
//! plans/cmux-next/chrome-parity/p05-ipc.md 3.6).
//!
//! A workspace registry revision (the ledger row, its event, and with
//! `project_resource` the resource projection) is prepared on the request
//! thread and admitted on the journal writer's durable lane. The replay
//! check, the revision CAS and the revision are decided at prepare
//! (p05-ipc-132); the writer commits the rows later in admit order, so the
//! request never waits for a writer receipt. Rows a commit also writes
//! (presentation, a creation's state mark) are owned and ride the same
//! intent, so they land in the same SAVEPOINT. Chromium orders work when it
//! is scheduled and lets the backing store only apply it
//! (content/browser/indexed_db/indexed_db_transaction_coordinator.cc).
//!
//! The F10 core falls back to commit and wait (counted by reason) when the
//! feature is off, the writer does not run, or this thread holds the
//! registry connection (a caller that pinned it before taking state).

use super::*;

impl WorkspaceRegistry {
    /// Prepare one workspace registry revision and admit it; the commit the
    /// reply carries is the one decided at prepare (or, after a counted
    /// fallback, the committed one). `extra` rows are written after the
    /// ledger, before the resource batch, in the same transaction.
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub(super) fn admit_workspace_registry(
        &mut self,
        mutation: &WorkspaceMutation,
        fingerprint: &Value,
        expected_generation: Option<&str>,
        expected_revision: Option<u64>,
        event_kind: &str,
        workspace_key: &str,
        workspaces: &[RegistryWorkspace],
        active_workspace: Option<&WorkspacePublicId>,
        result: &Value,
        project_resource: bool,
        extra: Option<OwnedTransactionWrite>,
    ) -> anyhow::Result<RegistryCommit> {
        let intent = self.prepare_workspace_commit(
            mutation,
            fingerprint,
            expected_generation,
            expected_revision,
            event_kind,
            workspace_key,
            workspaces,
            active_workspace,
            result,
            project_resource,
        )?;
        let intent = match extra {
            Some(extra) => intent.with_extra_rows(extra),
            None => intent,
        };
        Ok(self
            .admit_and_enqueue(RegistryIntent::Workspace(intent))?
            .receipt
            .into_workspace()?
            .commit)
    }
}

#[cfg(test)]
#[path = "workspace_admit_tests.rs"]
mod tests;
