//! Terminal record commits (`commit_terminal`: reserved, ready, adopting,
//! exited, tombstoned) as journal writer intents (PR5 of
//! plans/cmux-tui-journal-write-path.md).
//!
//! The request thread validates the record and renders its canonical JSON
//! while it holds the registry, and admits it: the replay check and the CAS
//! on the terminal revision run in memory (`mutation_ledger_cache`). Every
//! write runs inside the commit transaction at the admitted revision (the
//! writer's SAVEPOINT, or a request-side transaction when the writer does
//! not run).

use super::mutation_ledger::LedgerOutcome;
use super::registry_intent::LedgerIntent;
use super::*;

/// One prepared `commit_terminal`.
#[derive(Debug)]
pub(crate) struct TerminalCommitIntent {
    mutation: WorkspaceMutation,
    /// Canonical fingerprint.
    fingerprint: String,
    expected_generation: Option<String>,
    /// The registry generation at prepare.
    generation: String,
    expected_revision: Option<u64>,
    event_kind: String,
    terminal: RegistryTerminal,
    result: Value,
    result_json: String,
    launch_spec_json: String,
    exit_json: Option<String>,
    /// The revision the ledger cache admitted this commit at
    /// (`mutation_ledger_cache`); `None` commits at the stored next one.
    admitted_revision: Option<u64>,
}

impl WorkspaceRegistry {
    /// Validate one terminal transition for [`Self::commit_terminal`].
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub(crate) fn prepare_terminal_commit(
        &self,
        mutation: &WorkspaceMutation,
        fingerprint: &Value,
        expected_generation: Option<&str>,
        expected_revision: Option<u64>,
        event_kind: &str,
        terminal: &RegistryTerminal,
        result: &Value,
    ) -> anyhow::Result<TerminalCommitIntent> {
        self.connection.drain_unsettled()?;
        validate_identifier("mutation id", &mutation.id)?;
        validate_identifier("mutation origin", &mutation.origin)?;
        validate_identifier("terminal event kind", event_kind)?;
        validate_terminal(terminal)?;
        let launch_spec_json = canonical_json(&durable_launch_spec(terminal))?;
        if launch_spec_json.len() > MAX_LAUNCH_SPEC_BYTES {
            anyhow::bail!("terminal launch spec exceeds {MAX_LAUNCH_SPEC_BYTES} bytes");
        }
        Ok(TerminalCommitIntent {
            mutation: mutation.clone(),
            fingerprint: canonical_json(fingerprint)?,
            expected_generation: expected_generation.map(str::to_string),
            generation: self.generation.clone(),
            expected_revision,
            event_kind: event_kind.to_string(),
            terminal: terminal.clone(),
            result: result.clone(),
            result_json: canonical_json(result)?,
            launch_spec_json,
            exit_json: terminal.exit.as_ref().map(canonical_json).transpose()?,
            admitted_revision: None,
        })
    }
}

impl WorkspaceRegistry {
    /// Commit several prepared terminal transitions in one request-side
    /// transaction (one fsync), each under its own SAVEPOINT: an intent that
    /// fails rolls back alone and gets its own error. The caller holds the
    /// registry across prepare and commit, so no other writer runs between.
    ///
    /// Each intent is admitted just before it applies and a failed one is
    /// settled at once, so the next one reuses its revision: the revisions
    /// stay contiguous, as when each read the stored one.
    #[track_caller]
    pub(crate) fn commit_terminals_together(
        &mut self,
        intents: Vec<TerminalCommitIntent>,
    ) -> anyhow::Result<Vec<Result<TerminalRegistryCommit, String>>> {
        self.ledger_cache.reconcile_indeterminate();
        let connection = std::sync::Arc::clone(&self.connection);
        let db = connection.get();
        let tx = db.unchecked_transaction()?;
        let mut commits = Vec::with_capacity(intents.len());
        let mut applied = Vec::with_capacity(intents.len());
        let mut failure = None;
        for mut intent in intents {
            let commit = match self.admit_ledger_intent(&mut intent) {
                Ok(Some(replayed)) => {
                    replayed.into_terminal().map_err(|error| format!("{error:#}"))
                }
                Ok(None) => {
                    let ticket = intent.ledger_request().ticket();
                    match RegistryIntent::Terminal(intent).apply_in_savepoint(&tx) {
                        Ok(commit) => {
                            let commit = commit.and_then(|receipt| {
                                receipt.into_terminal().map_err(|error| format!("{error:#}"))
                            });
                            match &commit {
                                Ok(commit) if !commit.replayed => {
                                    applied.push((ticket, commit.revision));
                                }
                                _ => self.ledger_cache.settle(&ticket, LedgerOutcome::Unapplied),
                            }
                            commit
                        }
                        Err(error) => {
                            self.ledger_cache.settle(&ticket, LedgerOutcome::Unapplied);
                            failure = Some(error);
                            break;
                        }
                    }
                }
                Err(error) => Err(format!("{error:#}")),
            };
            commits.push(commit);
        }
        let committed = match failure {
            Some(error) => {
                drop(tx);
                Err(error)
            }
            None => tx.commit().map_err(anyhow::Error::from),
        };
        drop(db);
        for (ticket, revision) in applied {
            let outcome = match committed {
                Ok(()) => LedgerOutcome::Committed(revision),
                Err(_) => LedgerOutcome::Unapplied,
            };
            self.ledger_cache.settle(&ticket, outcome);
        }
        committed?;
        self.connection.write_path_stats().request_effect_committed();
        Ok(commits)
    }
}

impl LedgerIntent for TerminalCommitIntent {
    fn ledger_request(&self) -> mutation_ledger::LedgerRequest<'_> {
        mutation_ledger::LedgerRequest {
            ledger: mutation_ledger::KeyedLedger::Terminal,
            mutation: &self.mutation,
            fingerprint: &self.fingerprint,
            result_json: &self.result_json,
            expected_revision: self.expected_revision,
        }
    }

    fn check_generation(&self) -> anyhow::Result<()> {
        if let Some(expected) = self.expected_generation.as_deref()
            && expected != self.generation
        {
            anyhow::bail!(
                "terminal generation conflict: expected {expected}, current {}",
                self.generation
            );
        }
        Ok(())
    }

    fn admit_at(&mut self, revision: u64) {
        self.admitted_revision = Some(revision);
    }

    fn admitted_revision(&self) -> Option<u64> {
        self.admitted_revision
    }

    fn replayed(&self, revision: u64, result_json: &str) -> anyhow::Result<RegistryReceipt> {
        Ok(RegistryReceipt::Terminal(TerminalRegistryCommit {
            revision,
            result: serde_json::from_str(result_json)?,
            replayed: true,
        }))
    }
}

impl TerminalCommitIntent {
    /// Resident size for the writer's durable batch byte cap.
    pub(crate) fn estimated_bytes(&self) -> usize {
        self.fingerprint
            .len()
            .saturating_add(self.result_json.len().saturating_mul(2))
            .saturating_add(self.launch_spec_json.len().saturating_mul(2))
            .saturating_add(self.exit_json.as_ref().map_or(0, String::len))
            .saturating_add(1024)
    }

    /// The body of `commit_terminal` inside the caller's transaction.
    pub(super) fn apply(&self, tx: &Transaction<'_>) -> anyhow::Result<TerminalRegistryCommit> {
        let (mutation, terminal) = (&self.mutation, &self.terminal);
        // An admitted intent's `meta` writes are the revision the cache gave it.
        let _admitted =
            self.admitted_revision.map(|_| mutation_ledger::AdmittedWriteScope::enter());
        // A key older than the ledger cache replays here.
        if let Some(replay) = terminal_replay(tx, mutation, &self.fingerprint)? {
            return Ok(replay);
        }
        if let Some(expected) = self.expected_generation.as_deref()
            && expected != self.generation
        {
            anyhow::bail!(
                "terminal generation conflict: expected {expected}, current {}",
                self.generation
            );
        }
        let current_revision = transaction_terminal_revision(tx)?;
        let plan = match self.admitted_revision {
            Some(revision) => mutation_ledger::RevisionPlan::Admitted(revision),
            None => mutation_ledger::RevisionPlan::Next { expected: self.expected_revision },
        };
        let revision = plan.revision(mutation_ledger::KeyedLedger::Terminal, current_revision)?;
        let existing = read_terminal(tx, &terminal.terminal_id)?;
        if let Some(existing) = existing.as_ref()
            && existing.lifecycle == TerminalLifecycle::Exited
            && terminal.lifecycle == TerminalLifecycle::Exited
        {
            if existing.incarnation != terminal.incarnation {
                anyhow::bail!("terminal_incarnation_mismatch");
            }
            // Process exit is a latch: the first observed reason/status is
            // authoritative. Reader EOF, child wait, and reconnect failure can
            // race, but later observations neither rewrite metadata nor mint a
            // new durable revision/event.
            return Ok(TerminalRegistryCommit {
                revision: current_revision,
                result: self.result.clone(),
                replayed: true,
            });
        }
        validate_terminal_transition(existing.as_ref(), terminal)?;
        if terminal.lifecycle != TerminalLifecycle::Tombstoned
            && existing.as_ref().is_none_or(|stored| stored.workspace_key != terminal.workspace_key)
        {
            require_live_workspace(tx, &terminal.workspace_key)?;
        }

        let sqlite_revision =
            i64::try_from(revision).context("terminal revision exceeds SQLite integer range")?;
        tx.execute(
            "INSERT INTO terminal_hosts(
               terminal_id, workspace_key, incarnation, lifecycle, launch_spec_json,
               exit_json, on_exit, created_revision, updated_revision, deleted_revision
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)
             ON CONFLICT(terminal_id) DO UPDATE SET
               workspace_key=excluded.workspace_key,
               incarnation=excluded.incarnation,
               lifecycle=excluded.lifecycle,
               launch_spec_json=excluded.launch_spec_json,
               exit_json=excluded.exit_json,
               on_exit=excluded.on_exit,
               updated_revision=excluded.updated_revision,
               deleted_revision=excluded.deleted_revision",
            params![
                terminal.terminal_id,
                terminal.workspace_key,
                terminal.incarnation,
                terminal.lifecycle.as_str(),
                self.launch_spec_json,
                self.exit_json,
                terminal.on_exit.column(),
                sqlite_revision,
                (terminal.lifecycle == TerminalLifecycle::Tombstoned).then_some(sqlite_revision),
            ],
        )?;
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'terminal_revision'",
            [revision.to_string()],
        )?;
        let ledger = mutation_ledger::KeyedLedger::Terminal;
        let row = (self.fingerprint.as_str(), self.result_json.as_str(), sqlite_revision);
        mutation_ledger::insert_keyed_mutation(tx, ledger, mutation, row.0, row.1, row.2)?;
        tx.execute(
            "INSERT INTO terminal_events(
               revision, kind, terminal_id, workspace_key, origin, mutation_id, result_json
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                sqlite_revision,
                self.event_kind,
                terminal.terminal_id,
                terminal.workspace_key,
                mutation.origin,
                mutation.id,
                self.result_json,
            ],
        )?;
        detached_terminals::keep_reserved_detached(tx, existing.as_ref(), terminal)?;
        Ok(TerminalRegistryCommit { revision, result: self.result.clone(), replayed: false })
    }
}
