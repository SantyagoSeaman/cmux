//! Committing a prepared resource effect, and staging a legacy workspace row inside one.

use super::*;

impl WorkspaceRegistry {
    /// Stage a legacy workspace row inside a prepared resource effect.
    ///
    /// The outer effect must subsequently commit a full resource projection.
    /// This stage deliberately leaves the public revision and event stream
    /// untouched so one logical creation produces one public batch.
    /// Production stages (`Mux` workspace creation and close) prepare with
    /// `project_resource` false and admit the intent themselves, so they can
    /// release their connection pin (`workspace_admit`); `extra` rows (state
    /// marks) ride the intent.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub(crate) fn commit_for_resource_effect(
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
            false,
        )?;
        let intent = match extra {
            Some(extra) => intent.with_extra_rows(extra),
            None => intent,
        };
        Ok(self.admit_workspace_commit(intent, false, || {})?.commit)
    }

    /// Prepare a `commit_resource_effect` intent that also writes `extra` in
    /// its transaction (a notification's local feed rows, B2). The caller
    /// commits it through the journal writer (`Mux::commit_effect_intent`):
    /// the receipt and the extra rows land under one SAVEPOINT of a writer
    /// batch, or in one request-side transaction when the writer is not
    /// running.
    #[track_caller]
    pub(crate) fn prepare_effect_outcome_intent_with(
        &self,
        idempotency_key: &str,
        operation: &str,
        fingerprint: &Value,
        outcome: &ResourceEffectOutcome,
        deltas: Option<&Value>,
        extra: OwnedTransactionWrite,
    ) -> anyhow::Result<(EffectCommitIntent, EffectCommitFinish)> {
        let (intent, finish) = self.prepare_effect_outcome_intent(
            idempotency_key,
            operation,
            fingerprint,
            outcome,
            deltas,
        )?;
        Ok((intent.with_extra_rows(extra), finish))
    }
}
