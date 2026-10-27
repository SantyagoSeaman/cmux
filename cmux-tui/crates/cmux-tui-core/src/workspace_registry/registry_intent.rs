//! Registry commits as journal writer intents (plans/cmux-tui-journal-write-path.md:
//! PR4b effect receipts; PR5 terminal records and workspace registry commits).
//!
//! A request thread prepares a [`RegistryIntent`] while it holds the
//! workspace registry: validation, canonical JSON and any reads stay
//! request-side. The journal writer applies the intent inside its batch
//! transaction under a SAVEPOINT, so many commits share one fsync, and
//! answers with a [`RegistryReceipt`].
//!
//! Before it sends a workspace or terminal intent, the request admits it
//! against the registry's in-memory ledger cache (`mutation_ledger_cache`):
//! a retried mutation replays there, the revision CAS runs there, and the
//! intent carries the revision the writer must write. The receipt settles
//! the cache entry (committed, unapplied, or indeterminate until drained).
//!
//! Lock order is unchanged: workspace registry -> registry connection ->
//! state. While it waits for the receipt the request thread holds the
//! registry (topology writers stay serialized) and may hold state, but never
//! the connection: the writer needs it, and the writer takes neither the
//! registry nor state (`server-stats` `write_path.writer_registry_locks`).
//!
//! An indeterminate receipt (the writer admitted the batch, the result did
//! not arrive in time) is kept on the connection with the state update the
//! request could not apply. The next request that pins state settles it
//! (`Mux::lock_state_pinned`): a committed receipt applies its state update
//! and raises the in-memory revisions to the journal head (cx-g1fa.3).

use std::sync::Arc;

use super::*;
use crate::journal_ingress::{EffectSend, PendingRegistryReceipt};

/// The SAVEPOINT that scopes one intent inside a writer batch.
const REGISTRY_INTENT_SAVEPOINT: &str = "cmux_registry_intent";

/// Extra rows an intent writes in its transaction. `Fn`, not `FnOnce`: the
/// writer can retry or split a failed batch and apply the intent again.
pub(crate) type OwnedTransactionWrite =
    Box<dyn Fn(&Transaction<'_>) -> anyhow::Result<()> + Send + Sync>;

/// The state update of a request whose receipt was indeterminate, applied
/// when the receipt arrives (see the module docs).
pub(crate) type SettleState = Box<dyn FnOnce(&mut crate::State, &RegistryReceipt) + Send>;

/// One registry commit, ready for any thread that holds the connection: the
/// journal writer batch, or a request-side transaction.
#[derive(Debug)]
pub(crate) enum RegistryIntent {
    Effect(EffectCommitIntent),
    Terminal(TerminalCommitIntent),
    Workspace(WorkspaceCommitIntent),
}

/// The writer's (or a local transaction's) answer for one intent.
#[derive(Debug)]
pub(crate) enum RegistryReceipt {
    Effect(EffectCommitReceipt),
    Terminal(TerminalRegistryCommit),
    Workspace(WorkspaceCommitReceipt),
}

/// Where requests send registry intents: the journal writer, installed by
/// the mux when its writer starts.
pub(crate) trait RegistryIntentSink: Send + Sync {
    /// True when the journal writer runs and the caller is not its thread.
    fn accepts(&self) -> bool;
    /// Commit `intent` through the writer. `site` is the registry call that
    /// committed it: the dyn call breaks `#[track_caller]`, so the receipt
    /// wait is noted there (`request_path`) explicitly.
    fn send(
        &self,
        intent: RegistryIntent,
        site: &'static std::panic::Location<'static>,
    ) -> EffectSend;
}

/// An indeterminate receipt and the state update it still owes.
pub(crate) struct UnsettledReceipt {
    pending: PendingRegistryReceipt,
    /// The writer's answer once it arrived (see `drain_unsettled`).
    resolved: Option<Result<RegistryReceipt, String>>,
    settle: Option<SettleState>,
}

/// How long a new registry commit waits for an earlier indeterminate one.
const UNSETTLED_DRAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// A workspace or terminal intent that the in-memory ledger admits before
/// it is written (`mutation_ledger_cache`).
pub(crate) trait LedgerIntent {
    fn ledger_request(&self) -> mutation_ledger::LedgerRequest<'_>;
    /// The generation CAS: after the replay check, before the revision CAS
    /// (the order the commit transaction used).
    fn check_generation(&self) -> anyhow::Result<()>;
    /// Commit at `revision` (the writer has no CAS left).
    fn admit_at(&mut self, revision: u64);
    fn admitted_revision(&self) -> Option<u64>;
    /// The receipt of a replayed mutation.
    fn replayed(&self, revision: u64, result_json: &str) -> anyhow::Result<RegistryReceipt>;
}

/// The ledger outcome of an answered commit.
fn ledger_outcome(result: &anyhow::Result<RegistryReceipt>) -> mutation_ledger::LedgerOutcome {
    use mutation_ledger::LedgerOutcome;
    match result {
        Ok(RegistryReceipt::Workspace(receipt)) if !receipt.commit.replayed => {
            LedgerOutcome::Committed(receipt.commit.revision)
        }
        Ok(RegistryReceipt::Terminal(commit)) if !commit.replayed => {
            LedgerOutcome::Committed(commit.revision)
        }
        _ => LedgerOutcome::Unapplied,
    }
}

/// How the writer (or a local transaction) answered an admitted intent.
enum SendOutcome {
    Answered(anyhow::Result<RegistryReceipt>),
    Indeterminate(anyhow::Error),
}

impl RegistryIntent {
    /// The cache entry an admitted intent settles.
    fn ledger_ticket(&self) -> Option<mutation_ledger::LedgerTicket> {
        let admitted: &dyn LedgerIntent = match self {
            Self::Effect(_) => return None,
            Self::Terminal(intent) => intent,
            Self::Workspace(intent) => intent,
        };
        admitted.admitted_revision().map(|_| admitted.ledger_request().ticket())
    }

    /// Resident size for the writer's durable batch byte cap.
    pub(crate) fn estimated_bytes(&self) -> usize {
        match self {
            Self::Effect(intent) => intent.estimated_bytes(),
            Self::Terminal(intent) => intent.estimated_bytes(),
            Self::Workspace(intent) => intent.estimated_bytes(),
        }
    }

    /// Apply inside `transaction` (no savepoint).
    fn apply(&self, transaction: &Transaction<'_>) -> anyhow::Result<RegistryReceipt> {
        Ok(match self {
            Self::Effect(intent) => RegistryReceipt::Effect(intent.apply(transaction)?),
            Self::Terminal(intent) => RegistryReceipt::Terminal(intent.apply(transaction)?),
            Self::Workspace(intent) => RegistryReceipt::Workspace(intent.apply(transaction, None)?),
        })
    }

    /// Apply inside a journal writer batch under its own SAVEPOINT.
    ///
    /// `Ok(Err(_))`: this intent failed (for example a revision conflict, or
    /// a row it reads is missing); its writes were rolled back and only its
    /// request gets the error. `Err(_)`: a failure of the connection itself
    /// (busy, locked, I/O, full, corrupt, the batch deadline interrupt; see
    /// [`fails_the_batch`]); the whole batch fails and the writer retries or
    /// splits it as for any other batch error.
    pub(crate) fn apply_in_savepoint(
        &self,
        transaction: &Transaction<'_>,
    ) -> anyhow::Result<Result<RegistryReceipt, String>> {
        transaction.execute_batch(&format!("SAVEPOINT {REGISTRY_INTENT_SAVEPOINT}"))?;
        match self.apply(transaction) {
            Ok(receipt) => {
                transaction.execute_batch(&format!("RELEASE {REGISTRY_INTENT_SAVEPOINT}"))?;
                Ok(Ok(receipt))
            }
            Err(error) if fails_the_batch(&error) => Err(error),
            Err(error) => {
                transaction.execute_batch(&format!(
                    "ROLLBACK TO {REGISTRY_INTENT_SAVEPOINT}; RELEASE {REGISTRY_INTENT_SAVEPOINT}"
                ))?;
                Ok(Err(format!("{error:#}")))
            }
        }
    }
}

impl RegistryReceipt {
    /// The resource revision this commit installed, if it advanced one.
    pub(crate) fn resource_revision(&self) -> Option<u64> {
        match self {
            Self::Effect(receipt) => Some(receipt.revision()),
            Self::Terminal(_) => None,
            Self::Workspace(receipt) => receipt.resource_revision,
        }
    }

    pub(crate) fn into_effect(self) -> anyhow::Result<EffectCommitReceipt> {
        match self {
            Self::Effect(receipt) => Ok(receipt),
            _ => anyhow::bail!("registry receipt is not an effect receipt"),
        }
    }

    pub(crate) fn into_terminal(self) -> anyhow::Result<TerminalRegistryCommit> {
        match self {
            Self::Terminal(commit) => Ok(commit),
            _ => anyhow::bail!("registry receipt is not a terminal commit"),
        }
    }

    pub(crate) fn into_workspace(self) -> anyhow::Result<WorkspaceCommitReceipt> {
        match self {
            Self::Workspace(receipt) => Ok(receipt),
            _ => anyhow::bail!("registry receipt is not a workspace registry commit"),
        }
    }
}

/// True when `error` is a failure of the connection or the batch (busy,
/// locked, I/O, full, corrupt, interrupted by the batch deadline) rather
/// than of this one intent. Data errors (a missing row, a constraint, a
/// conversion) fail only the intent, as they failed only its request when
/// it committed alone.
fn fails_the_batch(error: &anyhow::Error) -> bool {
    use rusqlite::ErrorCode;
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(failure, _)) if matches!(
                failure.code,
                ErrorCode::DatabaseBusy
                    | ErrorCode::DatabaseLocked
                    | ErrorCode::OperationInterrupted
                    | ErrorCode::SystemIoFailure
                    | ErrorCode::DiskFull
                    | ErrorCode::DatabaseCorrupt
                    | ErrorCode::NotADatabase
                    | ErrorCode::OutOfMemory
                    | ErrorCode::CannotOpen
                    | ErrorCode::FileLockingProtocolFailed
                    | ErrorCode::ReadOnly
            )
        )
    })
}

impl RegistryConnection {
    /// Install the journal writer as the target of registry intents. The
    /// first install wins (one writer per session database).
    pub(crate) fn install_intent_sink(&self, sink: Arc<dyn RegistryIntentSink>) {
        let _ = self.intent_sink.set(sink);
    }

    fn accepting_sink(&self) -> Option<&Arc<dyn RegistryIntentSink>> {
        self.intent_sink.get().filter(|sink| sink.accepts())
    }

    fn hold_unsettled(&self, pending: PendingRegistryReceipt, settle: Option<SettleState>) {
        self.unsettled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(UnsettledReceipt { pending, resolved: None, settle });
    }

    /// Wait (bounded) until every earlier indeterminate commit has its
    /// answer, keeping the answers for the next state holder to settle. A
    /// new commit prepared before then could read a topology, a fold or a
    /// revision that the late commit is about to change. Errors when one is
    /// still unanswered after the wait.
    ///
    /// It holds the `unsettled` mutex across the wait. Callers must hold the
    /// workspace registry (all do): the writer takes neither the registry
    /// nor this mutex, so the wait cannot form a cycle. Each unanswered
    /// receipt it waits for is a `journal_wait` (`request_path`) at the
    /// caller.
    #[track_caller]
    pub(crate) fn drain_unsettled(&self) -> anyhow::Result<()> {
        let site = std::panic::Location::caller();
        let mut unsettled =
            self.unsettled.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = std::time::Instant::now() + UNSETTLED_DRAIN_WAIT;
        for entry in unsettled.iter_mut().filter(|entry| entry.resolved.is_none()) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            entry.resolved =
                entry.pending.try_take().or_else(|| entry.pending.wait_take(left, site));
            anyhow::ensure!(
                entry.resolved.is_some(),
                "an earlier session journal registry commit is still indeterminate; retry"
            );
        }
        Ok(())
    }

    /// Receipts that arrived for earlier indeterminate commits, with the
    /// state updates they owe. A receipt that resolved to an error owes
    /// nothing: the writer rolled the intent back. Unresolved ones stay.
    pub(crate) fn take_settled_receipts(&self) -> Vec<(RegistryReceipt, Option<SettleState>)> {
        let mut unsettled =
            self.unsettled.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if unsettled.is_empty() {
            return Vec::new();
        }
        let mut settled = Vec::new();
        let mut kept = Vec::with_capacity(unsettled.len());
        for mut entry in unsettled.drain(..) {
            match entry.resolved.take().or_else(|| entry.pending.try_take()) {
                None => kept.push(entry),
                Some(Ok(receipt)) => settled.push((receipt, entry.settle)),
                Some(Err(error)) => {
                    eprintln!("cmux-tui: an indeterminate registry commit did not commit: {error}");
                }
            }
        }
        *unsettled = kept;
        settled
    }
}

impl WorkspaceRegistry {
    /// Commit `intent` through the journal writer batch when the writer runs
    /// and this thread does not hold the connection; otherwise in its own
    /// request-side transaction. The caller holds the registry, and must not
    /// hold state unless it pinned the connection first.
    #[track_caller]
    pub(crate) fn commit_registry_intent(
        &mut self,
        intent: RegistryIntent,
    ) -> anyhow::Result<RegistryReceipt> {
        self.commit_registry_intent_with(intent, || {}, None)
    }

    /// [`Self::commit_registry_intent`] for a caller that holds state on a
    /// pinned connection. `release_pin` drops that pin once the intent goes
    /// to the writer (the caller keeps state across the receipt wait; the
    /// writer takes neither the registry nor state). `settle` is the state
    /// update to apply if the receipt is indeterminate and arrives later.
    #[track_caller]
    pub(crate) fn commit_registry_intent_with(
        &mut self,
        intent: RegistryIntent,
        release_pin: impl FnOnce(),
        settle: Option<SettleState>,
    ) -> anyhow::Result<RegistryReceipt> {
        let site = std::panic::Location::caller();
        // Every prepare drains earlier indeterminate commits first (bounded),
        // so `intent` is not built on a base a late commit changes; this
        // second drain covers one that went indeterminate meanwhile.
        self.connection.drain_unsettled()?;
        // Their answers are in SQLite now: the cache rereads its revisions.
        self.ledger_cache.reconcile_indeterminate();
        let mut intent = intent;
        if let Some(replayed) = self.admit_registry_intent(&mut intent)? {
            return Ok(replayed);
        }
        let ticket = intent.ledger_ticket();
        let (result, outcome) = match self.send_admitted_intent(intent, release_pin, settle, site) {
            SendOutcome::Answered(result) => {
                let outcome = ledger_outcome(&result);
                (result, outcome)
            }
            SendOutcome::Indeterminate(error) => {
                (Err(error), mutation_ledger::LedgerOutcome::Indeterminate)
            }
        };
        if let Some(ticket) = ticket {
            self.ledger_cache.settle(&ticket, outcome);
        }
        result
    }

    fn send_admitted_intent(
        &mut self,
        intent: RegistryIntent,
        release_pin: impl FnOnce(),
        settle: Option<SettleState>,
        site: &'static std::panic::Location<'static>,
    ) -> SendOutcome {
        let Some(sink) = self.connection.accepting_sink().cloned() else {
            return SendOutcome::Answered(self.commit_registry_intent_locally(&intent));
        };
        release_pin();
        // A thread that still holds the connection (an outer pin or guard)
        // would block the writer it waits for: commit locally instead. The
        // request_effect_commits counter makes any such caller visible.
        if self.connection.is_held_by_current_thread() {
            return SendOutcome::Answered(self.commit_registry_intent_locally(&intent));
        }
        match sink.send(intent, site) {
            EffectSend::Sent(Ok(receipt)) => SendOutcome::Answered(Ok(receipt)),
            EffectSend::Sent(Err(error)) => {
                self.abandon_effect_commit();
                SendOutcome::Answered(Err(error))
            }
            EffectSend::Indeterminate(error, pending) => {
                // The writer can still commit after this request gives up:
                // forget the public fold, and keep the receipt so the next
                // state holder installs what it committed.
                self.abandon_effect_commit();
                self.connection.hold_unsettled(pending, settle);
                SendOutcome::Indeterminate(error)
            }
            EffectSend::NotQueued(intent) => {
                // The writer stopped between the check and the send. A
                // thread that holds state cannot take the connection now.
                if registry_connection::current_thread_holds_state() {
                    return SendOutcome::Answered(Err(anyhow::anyhow!(
                        "the session journal writer stopped before it took a registry commit"
                    )));
                }
                SendOutcome::Answered(self.commit_registry_intent_locally(&intent))
            }
        }
    }

    /// Replay or admit a workspace or terminal intent (effects pass): the
    /// replay check and the revision CAS in memory, at the request.
    #[track_caller]
    fn admit_registry_intent(
        &mut self,
        intent: &mut RegistryIntent,
    ) -> anyhow::Result<Option<RegistryReceipt>> {
        match intent {
            RegistryIntent::Effect(_) => Ok(None),
            RegistryIntent::Terminal(intent) => self.admit_ledger_intent(intent),
            RegistryIntent::Workspace(intent) => self.admit_ledger_intent(intent),
        }
    }

    /// Replay `intent` from the ledger cache, or check its generation, CAS
    /// its expected revision against the newest admitted one and admit it at
    /// the next. A generation or revision conflict looks the key up in
    /// SQLite first, so the order stays replay, generation, CAS: a retry
    /// older than the cache still replays (after a restart too, when the
    /// generation changed). Returns the replayed receipt, if any.
    #[track_caller]
    pub(crate) fn admit_ledger_intent(
        &mut self,
        intent: &mut (impl LedgerIntent + ?Sized),
    ) -> anyhow::Result<Option<RegistryReceipt>> {
        if self.ledger_cache.is_stale() {
            self.ledger_cache.refresh(&self.connection.get())?;
        }
        let request = intent.ledger_request();
        if let Some(replay) = self.ledger_cache.replay(&request)? {
            return intent.replayed(replay.revision, &replay.result_json).map(Some);
        }
        if let Err(conflict) = intent.check_generation() {
            return self
                .replay_stored(intent)?
                .map_or(Err(conflict), |replayed| Ok(Some(replayed)));
        }
        let refusal = match self.ledger_cache.admit(&request) {
            Ok(mutation_ledger::Admission::Admitted(revision)) => {
                intent.admit_at(revision);
                return Ok(None);
            }
            Ok(mutation_ledger::Admission::Replay(replay)) => {
                return intent.replayed(replay.revision, &replay.result_json).map(Some);
            }
            Err(refusal) => refusal,
        };
        if !matches!(refusal, mutation_ledger::LedgerRefusal::RevisionConflict { .. }) {
            return Err(refusal.into());
        }
        self.replay_stored(intent)?.map_or(Err(refusal.into()), |replayed| Ok(Some(replayed)))
    }

    /// The replay of `intent` from its stored ledger row, for a key the
    /// cache no longer holds; `None` when it never committed.
    #[track_caller]
    fn replay_stored(
        &self,
        intent: &(impl LedgerIntent + ?Sized),
    ) -> anyhow::Result<Option<RegistryReceipt>> {
        let request = intent.ledger_request();
        let stored = mutation_ledger::stored_keyed_mutation(
            &self.connection.get(),
            request.ledger,
            request.mutation,
        )?;
        let Some((fingerprint, result_json, revision)) = stored else { return Ok(None) };
        let replay = request.replay_of(&fingerprint, result_json, revision)?;
        intent.replayed(replay.revision, &replay.result_json).map(Some)
    }

    /// Run before a store that writes `revision` or `terminal_revision` in
    /// its own transaction (`RevisionPlan::Next`): an admitted intent the
    /// writer may still commit must take its admitted revision first, or its
    /// commit would find that revision taken. Waits for those answers
    /// (bounded, as every prepare does) only while there are any.
    #[track_caller]
    pub(crate) fn settle_admissions_before_direct_write(&mut self) -> anyhow::Result<()> {
        if self.ledger_cache.has_unanswered() {
            self.connection.drain_unsettled()?;
            self.ledger_cache.reconcile_indeterminate();
        }
        Ok(())
    }

    /// Commit an intent in its own request-side transaction and fsync: the
    /// writer is disabled, stopped, or not running, or the caller holds the
    /// connection.
    #[track_caller]
    pub(crate) fn commit_registry_intent_locally(
        &self,
        intent: &RegistryIntent,
    ) -> anyhow::Result<RegistryReceipt> {
        let db = self.connection.get();
        let tx = db.unchecked_transaction()?;
        let receipt = intent.apply(&tx)?;
        tx.commit()?;
        drop(db);
        self.connection.write_path_stats().request_effect_committed();
        Ok(receipt)
    }
}

#[cfg(test)]
#[path = "registry_intent_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "registry_intent_ledger_tests.rs"]
mod ledger_tests;
