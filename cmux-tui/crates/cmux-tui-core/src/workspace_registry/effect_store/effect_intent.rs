//! Effect receipt commits as journal writer intents (PR4b of
//! plans/cmux-tui-journal-write-path.md).
//!
//! A request thread prepares an [`EffectCommitIntent`] while it holds the
//! workspace registry: validation, the stored fingerprint and public-fold
//! pruning stay request-side. The journal writer applies the intent inside
//! its batch transaction (`RegistryIntent::Effect`, see `registry_intent`),
//! so many receipts share one fsync, and answers with an
//! [`EffectCommitReceipt`]. The request thread then finishes the commit
//! (`finish_effect_commit`: projection stats and the public fold) under the
//! same registry hold.
//!
//! Lock order is unchanged: workspace registry -> registry connection ->
//! state. While it waits for the receipt the request thread holds the
//! registry, which keeps topology writers serialized, but neither the
//! connection (the writer needs it) nor state. The writer takes only the
//! connection; `server-stats` `write_path.writer_registry_locks` proves it.
//!
//! Prepare and executing transitions stay request-side: they must be durable
//! before the effect runs.
//!
//! Admit then reply (F10, p05-ipc-125): `admit_effect_commit` decides an
//! outcome's receipt before the writer applies the intent, so the request
//! can reply without waiting (`Mux::admit_effect_intent`); admissions the
//! store does not hold yet are tracked in `admitted_effects`.

use std::sync::Arc;

use super::admitted_effects::AdmittedReceipt;
use super::*;
use crate::user_settings::NewWorkspacePlacement;

/// One effect receipt commit, ready for any thread that holds the
/// connection: the journal writer batch, or a request-side transaction.
#[derive(Debug)]
pub(crate) struct EffectCommitIntent {
    idempotency_key: String,
    operation: String,
    /// The stored (redacted, canonical) fingerprint.
    fingerprint: String,
    generation: String,
    outcome_json: String,
    kind: EffectCommitKind,
    /// Rows written in the same transaction after the receipt (a
    /// notification's local feed rows, B2).
    extra: Option<ExtraRows>,
}

/// [`OwnedTransactionWrite`] with a `Debug` for the intent.
struct ExtraRows(OwnedTransactionWrite);

impl std::fmt::Debug for ExtraRows {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExtraRows")
    }
}

#[derive(Debug)]
enum EffectCommitKind {
    /// `commit_resource_effect`: an outcome, with optional public deltas.
    Outcome { outcome: ResourceEffectOutcome, outcome_value: Value, deltas: Option<Value> },
    /// `commit_resource_effect_patch`: a projected topology patch.
    /// `placement` is the request thread's `NewWorkspacePlacement` (a
    /// thread-scoped choice the writer thread cannot see), captured at
    /// prepare and scoped again where the patch applies.
    Patch {
        patch: ResourcePatch,
        result: Value,
        outcome: Value,
        deltas: Arc<Value>,
        placement: NewWorkspacePlacement,
    },
}

/// The writer's (or a local transaction's) answer for one intent.
#[derive(Debug)]
pub(crate) enum EffectCommitReceipt {
    /// `replayed`: the store already held this outcome; nothing was written.
    Outcome {
        revision: u64,
        replayed: bool,
    },
    Patch {
        commit: ResourcePatchCommit,
        spans: CommitSpans,
    },
}

/// What the request thread keeps to finish a commit after its receipt.
pub(crate) enum EffectCommitFinish {
    Outcome,
    Patch { deltas: Arc<Value>, restates_all: bool },
}

impl EffectCommitIntent {
    /// Resident size for the writer's durable batch byte cap.
    pub(crate) fn estimated_bytes(&self) -> usize {
        let fixed = self
            .idempotency_key
            .len()
            .saturating_add(self.operation.len())
            .saturating_add(self.fingerprint.len())
            .saturating_add(self.generation.len())
            .saturating_add(self.outcome_json.len())
            .saturating_add(512);
        match &self.kind {
            EffectCommitKind::Outcome { deltas, .. } => {
                fixed.saturating_add(self.outcome_json.len()).saturating_add(
                    deltas
                        .as_ref()
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                        .saturating_mul(256),
                )
            }
            EffectCommitKind::Patch { patch, deltas, .. } => fixed
                .saturating_add(self.outcome_json.len().saturating_mul(2))
                .saturating_add(patch.changes.len().saturating_mul(256))
                .saturating_add(deltas.as_array().map_or(0, Vec::len).saturating_mul(256)),
        }
    }

    /// Write `extra` in the same transaction as this receipt.
    pub(crate) fn with_extra_rows(mut self, extra: OwnedTransactionWrite) -> Self {
        self.extra = Some(ExtraRows(extra));
        self
    }

    /// The receipt of this intent at resource `revision`.
    fn receipt_at(&self, revision: u64, replayed: bool) -> EffectCommitReceipt {
        match &self.kind {
            EffectCommitKind::Outcome { .. } => EffectCommitReceipt::Outcome { revision, replayed },
            EffectCommitKind::Patch { result, .. } => EffectCommitReceipt::Patch {
                commit: ResourcePatchCommit { revision, result: result.clone(), replayed },
                spans: CommitSpans::default(),
            },
        }
    }

    /// Whether applying it advances the resource revision: a patch, or an
    /// outcome with public deltas (`commit_effect_outcome_in_transaction`).
    fn advances_resource_revision(&self) -> bool {
        match &self.kind {
            EffectCommitKind::Outcome { deltas, .. } => deltas.is_some(),
            EffectCommitKind::Patch { .. } => true,
        }
    }

    /// Apply inside `transaction` (the writer's savepoint, or a request-side
    /// transaction), then the extra rows.
    #[track_caller]
    pub(crate) fn apply(
        &self,
        transaction: &Transaction<'_>,
    ) -> anyhow::Result<EffectCommitReceipt> {
        let receipt = self.apply_receipt(transaction)?;
        if let Some(ExtraRows(extra)) = &self.extra {
            extra(transaction)?;
        }
        Ok(receipt)
    }

    #[track_caller]
    fn apply_receipt(&self, transaction: &Transaction<'_>) -> anyhow::Result<EffectCommitReceipt> {
        match &self.kind {
            EffectCommitKind::Outcome { outcome, outcome_value, deltas } => {
                let revision = commit_effect_outcome_in_transaction(
                    transaction,
                    &self.generation,
                    &self.idempotency_key,
                    &self.operation,
                    &self.fingerprint,
                    outcome,
                    outcome_value,
                    &self.outcome_json,
                    deltas.as_ref(),
                )?;
                Ok(EffectCommitReceipt::Outcome { revision, replayed: false })
            }
            EffectCommitKind::Patch { patch, result, outcome, deltas, placement } => {
                let (started, mut spans) = (std::time::Instant::now(), CommitSpans::default());
                let commit = placement.scoped(|| {
                    commit_resource_effect_patch_in_transaction(
                        transaction,
                        &self.generation,
                        &self.idempotency_key,
                        &self.operation,
                        &self.fingerprint,
                        patch,
                        result,
                        outcome,
                        &self.outcome_json,
                        deltas,
                        &mut spans,
                        false,
                    )
                })?;
                // The batch's shared fsync is not part of one intent's span.
                spans.total = started.elapsed();
                Ok(EffectCommitReceipt::Patch { commit, spans })
            }
        }
    }
}

impl WorkspaceRegistry {
    /// A sent intent returned an error. The writer may still commit it
    /// later (an indeterminate receipt), after this request released the
    /// registry: forget the public fold so the next commit cannot prune its
    /// deltas against a revision that is no longer the journal head.
    pub(crate) fn abandon_effect_commit(&mut self) {
        self.public_fold = None;
    }

    /// Validate and prepare a `commit_resource_effect` intent.
    #[track_caller]
    pub(crate) fn prepare_effect_outcome_intent(
        &self,
        idempotency_key: &str,
        operation: &str,
        fingerprint: &Value,
        outcome: &ResourceEffectOutcome,
        deltas: Option<&Value>,
    ) -> anyhow::Result<(EffectCommitIntent, EffectCommitFinish)> {
        // A late indeterminate commit must land before this intent's fold
        // pruning and checks read the base (registry_intent).
        self.connection.drain_unsettled()?;
        validate_identifier("idempotency key", idempotency_key)?;
        validate_identifier("resource operation", operation)?;
        let fingerprint = self.stored_fingerprint(fingerprint)?;
        let outcome_value = serde_json::to_value(outcome)?;
        let outcome_json = canonical_json(&outcome_value)?;
        let intent = EffectCommitIntent {
            idempotency_key: idempotency_key.to_string(),
            operation: operation.to_string(),
            fingerprint,
            generation: self.generation.clone(),
            outcome_json,
            extra: None,
            kind: EffectCommitKind::Outcome {
                outcome: outcome.clone(),
                outcome_value,
                deltas: deltas.cloned(),
            },
        };
        Ok((intent, EffectCommitFinish::Outcome))
    }

    /// Validate and prepare a `commit_resource_effect_patch` intent. Public
    /// `deltas` the journal already states are dropped here.
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub(crate) fn prepare_effect_patch_intent(
        &mut self,
        idempotency_key: &str,
        operation: &str,
        fingerprint: &Value,
        patch: &ResourcePatch,
        result: &Value,
        deltas: &Value,
        restates_all: bool,
    ) -> anyhow::Result<(EffectCommitIntent, EffectCommitFinish)> {
        // A late indeterminate commit must land before this intent's fold
        // pruning and checks read the base (registry_intent).
        self.connection.drain_unsettled()?;
        validate_identifier("idempotency key", idempotency_key)?;
        validate_identifier("resource operation", operation)?;
        validate_resource_patch(patch)?;
        #[cfg(test)]
        if self.resource_patch_failures_remaining.get() > 0 {
            self.resource_patch_failures_remaining
                .set(self.resource_patch_failures_remaining.get() - 1);
            anyhow::bail!("forced one-shot resource patch failure");
        }
        let fingerprint = self.stored_fingerprint(fingerprint)?;
        let outcome = serde_json::to_value(ResourceEffectOutcome::Success(result.clone()))?;
        let outcome_json = canonical_json(&outcome)?;
        let deltas = Arc::new(self.prune_stated_topology_deltas(deltas)?);
        let intent = EffectCommitIntent {
            idempotency_key: idempotency_key.to_string(),
            operation: operation.to_string(),
            fingerprint,
            generation: self.generation.clone(),
            outcome_json,
            extra: None,
            kind: EffectCommitKind::Patch {
                patch: patch.clone(),
                result: result.clone(),
                outcome,
                deltas: deltas.clone(),
                placement: NewWorkspacePlacement::current(),
            },
        };
        Ok((intent, EffectCommitFinish::Patch { deltas, restates_all }))
    }

    /// Commit `intent`: admitted with the receipt decided at prepare when it
    /// can be (admit then reply; the journal writer applies it later, in
    /// admit order), or through the writer, waiting for its receipt. An
    /// effect the store or a tracked admission already committed with the
    /// same outcome replays that receipt (`replayed`) and writes nothing.
    #[track_caller]
    pub(crate) fn admit_effect_commit(
        &mut self,
        intent: EffectCommitIntent,
    ) -> anyhow::Result<EffectCommitReceipt> {
        match self.prepare_effect_receipt(&intent)? {
            PreparedEffect::Replayed(receipt) => Ok(receipt),
            PreparedEffect::CommitAndWait => {
                // The tracked head does not follow a commit it did not admit.
                self.admitted_effects.forget_head();
                self.commit_registry_intent(RegistryIntent::Effect(intent))?.into_effect()
            }
            PreparedEffect::Admit(prepared) => {
                let key = intent.idempotency_key.clone();
                let tracked = AdmittedReceipt {
                    outcome_json: intent.outcome_json.clone(),
                    revision: prepared.revision(),
                };
                let admission = self.admit_registry_intent(
                    RegistryIntent::Effect(intent),
                    RegistryReceipt::Effect(prepared),
                    || {},
                    None,
                )?;
                match admission.sequence() {
                    Some(sequence) => self.admitted_effects.admitted(key, sequence, tracked),
                    // Committed through the writer instead (a counted fallback).
                    None => self.admitted_effects.forget_head(),
                }
                admission.into_receipt().into_effect()
            }
        }
    }

    /// Decide how `intent` commits, running the checks of the writer's apply
    /// on what the writer will apply it to.
    ///
    /// The resource head comes from the store when it holds every admitted
    /// intent, or from the tracked admissions when each unanswered one is
    /// tracked (`AdmittedEffects`): then this key's rows are final in SQLite
    /// but for its own tracked commit, so they are read past the barrier.
    /// Otherwise the read waits behind the barrier; if the barrier gives up
    /// first, the head is unknown and the intent commits and waits behind
    /// the admitted ones on the writer's lane. A patch always commits and
    /// waits: its apply has refusals (ephemeral moves, constraints) only the
    /// writer's transaction finds.
    #[track_caller]
    fn prepare_effect_receipt(
        &mut self,
        intent: &EffectCommitIntent,
    ) -> anyhow::Result<PreparedEffect> {
        let site = std::panic::Location::caller();
        let key = intent.idempotency_key.as_str();
        // The caller holds the registry, and admitting needs it: this count
        // and the unanswered set stay put until the intent is admitted.
        let admitted = self.connection.admit.snapshot().admitted;
        let (rows, head) = if !self.connection.admit.has_unresolved() {
            let rows = EffectRows::read(&self.connection.get_at(site), key)?;
            self.admitted_effects.settle(admitted, rows.resource_revision);
            let head = rows.resource_revision;
            (rows, Some(head))
        } else if let Some(tracked) = self.admitted_effects.head(admitted) {
            self.admitted_effects.stats_mut().tracked_prepares += 1;
            let rows = EffectRows::read(&self.connection.get_unbarriered_at(site), key)?;
            let head = rows.resource_revision.max(tracked);
            (rows, Some(head))
        } else {
            self.admitted_effects.stats_mut().barrier_prepares += 1;
            let rows = EffectRows::read(&self.connection.get_at(site), key)?;
            if self.connection.admit.has_unresolved() {
                (rows, None)
            } else {
                self.admitted_effects.settle(admitted, rows.resource_revision);
                let head = rows.resource_revision;
                (rows, Some(head))
            }
        };
        require_effect_identity(
            key,
            &intent.operation,
            &intent.fingerprint,
            &rows.operation,
            &rows.fingerprint,
        )?;
        let committed = match self.admitted_effects.receipt(key) {
            Some(admitted) => Some((admitted.outcome_json.as_str(), admitted.revision)),
            None => match (rows.state.as_str(), &rows.committed) {
                ("executing", _) => None,
                ("committed", Some((outcome_json, revision))) => {
                    Some((outcome_json.as_str(), *revision))
                }
                (state, _) => {
                    anyhow::bail!("resource effect {key:?} cannot commit from state {state:?}")
                }
            },
        };
        if let Some((outcome_json, revision)) = committed {
            anyhow::ensure!(
                outcome_json == intent.outcome_json,
                "resource effect {key:?} cannot commit from state \"committed\""
            );
            return Ok(PreparedEffect::Replayed(intent.receipt_at(revision, true)));
        }
        anyhow::ensure!(
            rows.creations == 0 || rows.executing_creations == 1,
            "correlated resource effect {key:?} could not commit its outcome"
        );
        let head = match (head, &intent.kind) {
            (_, EffectCommitKind::Patch { .. }) => {
                self.admitted_effects.stats_mut().waited_patch += 1;
                return Ok(PreparedEffect::CommitAndWait);
            }
            (None, EffectCommitKind::Outcome { .. }) => {
                self.admitted_effects.stats_mut().waited_unsettled += 1;
                return Ok(PreparedEffect::CommitAndWait);
            }
            (Some(head), EffectCommitKind::Outcome { .. }) => head,
        };
        let revision = if intent.advances_resource_revision() {
            head.checked_add(1).ok_or_else(|| anyhow::anyhow!("resource revision exhausted"))?
        } else {
            head
        };
        Ok(PreparedEffect::Admit(intent.receipt_at(revision, false)))
    }

    /// How effect prepares found their receipts (`AdmittedEffects`).
    #[cfg(test)]
    pub(crate) fn admitted_effect_stats(&self) -> admitted_effects::AdmittedEffectStats {
        self.admitted_effects.stats()
    }

    /// Commit an intent in its own request-side transaction and fsync. The
    /// journal writer path is preferred (`Mux::commit_effect_intent`); this
    /// runs when the writer is disabled, stopped, or not running.
    #[track_caller]
    pub(crate) fn commit_effect_intent_locally(
        &self,
        intent: EffectCommitIntent,
    ) -> anyhow::Result<EffectCommitReceipt> {
        self.commit_registry_intent_locally(&RegistryIntent::Effect(intent))?.into_effect()
    }

    /// Request-side bookkeeping after a durable receipt: projection commit
    /// spans and the public fold. The caller held the registry since prepare.
    #[track_caller]
    pub(crate) fn finish_effect_commit(
        &mut self,
        finish: EffectCommitFinish,
        receipt: EffectCommitReceipt,
    ) -> anyhow::Result<EffectCommitReceipt> {
        match (finish, receipt) {
            (EffectCommitFinish::Outcome, receipt @ EffectCommitReceipt::Outcome { .. }) => {
                Ok(receipt)
            }
            (
                EffectCommitFinish::Patch { deltas, restates_all },
                EffectCommitReceipt::Patch { commit, spans },
            ) => {
                self.resource_projection_stats.committed(spans);
                self.record_public_fold(
                    commit.revision.saturating_sub(1),
                    commit.revision,
                    &deltas,
                    restates_all,
                );
                Ok(EffectCommitReceipt::Patch { commit, spans })
            }
            _ => anyhow::bail!("effect commit receipt does not match its intent"),
        }
    }
}

impl EffectCommitReceipt {
    pub(crate) fn revision(&self) -> u64 {
        match self {
            Self::Outcome { revision, .. } => *revision,
            Self::Patch { commit, .. } => commit.revision,
        }
    }

    /// The store already held this effect committed with the same outcome.
    pub(crate) fn replayed(&self) -> bool {
        match self {
            Self::Outcome { replayed, .. } => *replayed,
            Self::Patch { commit, .. } => commit.replayed,
        }
    }

    pub(crate) fn into_patch_commit(self) -> anyhow::Result<ResourcePatchCommit> {
        match self {
            Self::Patch { commit, .. } => Ok(commit),
            Self::Outcome { .. } => anyhow::bail!("effect commit receipt is not a patch commit"),
        }
    }
}

/// How [`WorkspaceRegistry::prepare_effect_receipt`] commits an intent.
enum PreparedEffect {
    /// Admit it with this receipt.
    Admit(EffectCommitReceipt),
    /// The store or a tracked admission already holds it: this receipt,
    /// nothing to write.
    Replayed(EffectCommitReceipt),
    /// Commit it through the writer and wait for the writer's receipt.
    CommitAndWait,
}

/// The rows an effect commit checks, as one read finds them.
struct EffectRows {
    operation: String,
    fingerprint: String,
    state: String,
    /// The outcome and revision of a committed row.
    committed: Option<(String, u64)>,
    /// Correlated creation receipts, and those an effect is executing.
    creations: i64,
    executing_creations: i64,
    resource_revision: u64,
}

impl EffectRows {
    fn read(connection: &Connection, idempotency_key: &str) -> anyhow::Result<Self> {
        let row = connection
            .query_row(
                "SELECT operation, fingerprint, state, outcome_json, committed_revision
                 FROM resource_effect_receipts
                 WHERE idempotency_key = ?1",
                [idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!("resource effect intent {idempotency_key:?} is missing")
            })?;
        let (operation, fingerprint, state, outcome_json, committed_revision) = row;
        let committed = match (outcome_json, committed_revision) {
            (Some(outcome_json), Some(revision)) => Some((
                outcome_json,
                u64::try_from(revision).context("committed resource revision is negative")?,
            )),
            _ => None,
        };
        let (creations, executing_creations) = connection.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(execution_kind = 'effect' AND state = 'executing'), 0)
             FROM resource_creation_receipts WHERE idempotency_key = ?1",
            [idempotency_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Self {
            operation,
            fingerprint,
            state,
            committed,
            creations,
            executing_creations,
            resource_revision: current_resource_revision(connection)?,
        })
    }
}

/// The body of `commit_resource_effect` inside a caller's transaction.
#[allow(clippy::too_many_arguments)]
fn commit_effect_outcome_in_transaction(
    tx: &Transaction<'_>,
    generation: &str,
    idempotency_key: &str,
    operation: &str,
    fingerprint: &str,
    outcome: &ResourceEffectOutcome,
    outcome_value: &Value,
    outcome_json: &str,
    deltas: Option<&Value>,
) -> anyhow::Result<u64> {
    let (stored_operation, stored_fingerprint, state, intent_json) =
        read_effect_record(tx, idempotency_key)?.ok_or_else(|| {
            anyhow::anyhow!("resource effect intent {idempotency_key:?} is missing")
        })?;
    require_effect_identity(
        idempotency_key,
        operation,
        fingerprint,
        &stored_operation,
        &stored_fingerprint,
    )?;
    anyhow::ensure!(
        state == "executing",
        "resource effect {idempotency_key:?} cannot commit from state {state:?}"
    );
    let previous_revision = transaction_resource_revision(tx)?;
    let revision = if let Some(deltas) = deltas {
        let revision = previous_revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("resource revision exhausted"))?;
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'resource_revision'",
            [revision.to_string()],
        )?;
        append_resource_journal_record(
            tx,
            revision,
            previous_revision,
            "resource-api",
            idempotency_key,
            operation,
            None,
            outcome_value,
            deltas,
        )?;
        resource_store::prune_resource_mutations(tx)?;
        revision
    } else {
        append_resource_effect_journal_record(
            tx,
            idempotency_key,
            operation,
            &serde_json::from_str(&intent_json)?,
            Some(outcome_value),
            match outcome {
                ResourceEffectOutcome::Success(_) => ResourceEffectJournalState::Succeeded,
                ResourceEffectOutcome::Failure(_) => ResourceEffectJournalState::Failed,
            },
        )?;
        previous_revision
    };
    tx.execute(
        "UPDATE resource_effect_receipts
         SET state = 'committed', outcome_json = ?2, committed_revision = ?3
         WHERE idempotency_key = ?1 AND state = 'executing'",
        params![
            idempotency_key,
            outcome_json,
            i64::try_from(revision).context("resource revision exceeds SQLite range")?,
        ],
    )?;
    let correlated = match outcome {
        ResourceEffectOutcome::Success(created_path) => tx.execute(
            "UPDATE resource_creation_receipts
             SET state = 'created', execution_generation = NULL,
                 created_path_json = ?2, generation = ?3, committed_revision = ?4
             WHERE idempotency_key = ?1 AND execution_kind = 'effect'
               AND state = 'executing'",
            params![
                idempotency_key,
                canonical_json(created_path)?,
                generation,
                i64::try_from(revision).context("resource revision exceeds SQLite range")?,
            ],
        )?,
        ResourceEffectOutcome::Failure(_) => tx.execute(
            "UPDATE resource_creation_receipts
             SET state = 'not_applied', execution_generation = NULL,
                 created_path_json = NULL, generation = NULL, committed_revision = NULL
             WHERE idempotency_key = ?1 AND execution_kind = 'effect'
               AND state = 'executing'",
            [idempotency_key],
        )?,
    };
    let creation_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM resource_creation_receipts WHERE idempotency_key = ?1",
        [idempotency_key],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        creation_count == 0 || correlated == 1,
        "correlated resource effect could not commit its outcome"
    );
    record_resource_input_receipt_completion(tx, idempotency_key, operation)?;
    Ok(revision)
}
