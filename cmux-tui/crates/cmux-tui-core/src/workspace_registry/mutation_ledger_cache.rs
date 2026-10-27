//! In-memory revisions and the mutation-ledger cache of the workspace and
//! terminal ledgers (F10 prereq; plans/cmux-next/chrome-parity/p05-ipc.md
//! section 3.6, task p05-ipc-132).
//!
//! A request admits its registry intent before any thread writes it: the
//! replay check (against the newest ledger rows and every admitted,
//! uncommitted intent), the revision CAS (against the newest admitted
//! revision) and the revision the intent commits at all run here, in memory,
//! under the workspace registry lock. The writer then writes exactly that
//! revision ([`RevisionPlan::Admitted`]): it has no CAS left to fail, and a
//! batch retry rewrites the same revision. Chromium orders IndexedDB the same
//! way: the coordinator decides order and conflicts when it schedules a
//! transaction, the backing store only applies it
//! (content/browser/indexed_db/indexed_db_transaction_coordinator.cc).
//!
//! The cache is seeded at open from `meta` and the newest rows of each
//! ledger. A key older than the cache still replays: the writer keeps its
//! in-transaction ledger check, and a revision conflict looks the key up in
//! SQLite before it refuses. Stores that still commit their own request-side
//! transactions advance a revision without admitting: the connection's update
//! hook marks the cache stale on any `meta` write outside an admitted apply,
//! and the next admission rereads both revisions.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context as _;
use rusqlite::Connection;
use rusqlite::hooks::Action;

use super::super::required_meta;
use super::{KeyedLedger, WorkspaceMutation};

/// Committed entries kept per ledger, newest first. Admitted entries are
/// kept until their commit answers, whatever their number.
pub(crate) const LEDGER_CACHE_CAPACITY: usize = 256;

thread_local! {
    /// True while this thread applies an admitted intent (its `meta`
    /// writes are the revisions the cache handed out).
    static ADMITTED_WRITE: Cell<bool> = const { Cell::new(false) };
}

/// Marks this thread's writes as an admitted intent's while it lives.
pub(crate) struct AdmittedWriteScope {
    previous: bool,
}

impl AdmittedWriteScope {
    pub(crate) fn enter() -> Self {
        Self { previous: ADMITTED_WRITE.with(|admitted| admitted.replace(true)) }
    }
}

impl Drop for AdmittedWriteScope {
    fn drop(&mut self) {
        ADMITTED_WRITE.with(|admitted| admitted.set(self.previous));
    }
}

/// The revision a commit writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevisionPlan {
    /// The next revision after the stored one, with an optional CAS: the
    /// stores that commit without admitting.
    Next { expected: Option<u64> },
    /// The revision the ledger cache admitted the intent at.
    Admitted(u64),
}

impl RevisionPlan {
    /// The revision to write over the stored `current`.
    pub(crate) fn revision(self, ledger: KeyedLedger, current: u64) -> anyhow::Result<u64> {
        match self {
            Self::Next { expected } => {
                if let Some(expected) = expected
                    && expected != current
                {
                    return Err(
                        LedgerRefusal::RevisionConflict { ledger, expected, current }.into()
                    );
                }
                current
                    .checked_add(1)
                    .ok_or_else(|| LedgerRefusal::RevisionExhausted { ledger }.into())
            }
            Self::Admitted(revision) => {
                anyhow::ensure!(
                    revision > current,
                    "admitted {} revision {revision} is not ahead of the committed {current}",
                    ledger.label()
                );
                Ok(revision)
            }
        }
    }
}

/// What an intent asks the cache.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LedgerRequest<'a> {
    pub(crate) ledger: KeyedLedger,
    pub(crate) mutation: &'a WorkspaceMutation,
    /// Canonical fingerprint.
    pub(crate) fingerprint: &'a str,
    /// Canonical result.
    pub(crate) result_json: &'a str,
    pub(crate) expected_revision: Option<u64>,
}

impl LedgerRequest<'_> {
    pub(crate) fn ticket(&self) -> LedgerTicket {
        LedgerTicket { ledger: self.ledger, key: ledger_key(self.mutation) }
    }

    fn different_payload(&self) -> LedgerRefusal {
        LedgerRefusal::DifferentPayload {
            ledger: self.ledger,
            origin: self.mutation.origin.clone(),
            mutation_id: self.mutation.id.clone(),
        }
    }

    /// The replay of a stored ledger row for this request.
    pub(crate) fn replay_of(
        &self,
        fingerprint: &str,
        result_json: String,
        revision: u64,
    ) -> Result<LedgerReplay, LedgerRefusal> {
        if fingerprint != self.fingerprint {
            return Err(self.different_payload());
        }
        Ok(LedgerReplay { revision, result_json })
    }
}

/// An admitted intent's place in the cache, kept to settle it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LedgerTicket {
    ledger: KeyedLedger,
    key: String,
}

/// How an admitted intent's commit answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LedgerOutcome {
    /// It committed at this revision.
    Committed(u64),
    /// It wrote nothing: refused, rolled back, or replayed by the writer.
    Unapplied,
    /// The writer may still commit it.
    Indeterminate,
}

/// A stored or admitted result for a retried mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LedgerReplay {
    pub(crate) revision: u64,
    pub(crate) result_json: String,
}

/// What [`LedgerCache::admit`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Admission {
    Replay(LedgerReplay),
    Admitted(u64),
}

/// Why the cache refused an intent; the messages are the stores' own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LedgerRefusal {
    DifferentPayload { ledger: KeyedLedger, origin: String, mutation_id: String },
    RevisionConflict { ledger: KeyedLedger, expected: u64, current: u64 },
    RevisionExhausted { ledger: KeyedLedger },
}

impl std::fmt::Display for LedgerRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DifferentPayload { ledger, origin, mutation_id } => {
                let prefix = match ledger {
                    KeyedLedger::Workspace => "",
                    KeyedLedger::Terminal => "terminal ",
                };
                write!(
                    formatter,
                    "{prefix}mutation {mutation_id} from {origin} was retried with a different payload"
                )
            }
            Self::RevisionConflict { ledger, expected, current } => write!(
                formatter,
                "{} revision conflict: expected {expected}, current {current}",
                ledger.label()
            ),
            Self::RevisionExhausted { ledger } => {
                write!(formatter, "{} revision exhausted", ledger.label())
            }
        }
    }
}

impl std::error::Error for LedgerRefusal {}

/// Identifiers carry no control character, so NUL separates them.
fn ledger_key(mutation: &WorkspaceMutation) -> String {
    format!("{}\u{0}{}", mutation.origin, mutation.id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Admitted,
    Indeterminate,
    Committed,
}

#[derive(Debug)]
struct LedgerEntry {
    fingerprint: String,
    result_json: String,
    revision: u64,
    state: EntryState,
}

/// One ledger's revisions and entries.
#[derive(Debug, Default)]
struct KeyedCache {
    /// The newest revision known committed.
    committed: u64,
    /// Revisions admitted and not yet answered.
    admitted: BTreeSet<u64>,
    entries: HashMap<String, LedgerEntry>,
    /// Committed keys, oldest first, for eviction.
    order: VecDeque<String>,
}

impl KeyedCache {
    fn seed(connection: &Connection, ledger: KeyedLedger) -> anyhow::Result<Self> {
        let mut cache = Self { committed: stored_revision(connection, ledger)?, ..Self::default() };
        let mut statement = connection.prepare(&format!(
            "SELECT origin, mutation_id, fingerprint, result_json, committed_revision
             FROM {} ORDER BY rowid DESC LIMIT ?1",
            ledger.table()
        ))?;
        let rows = statement
            .query_map([i64::try_from(LEDGER_CACHE_CAPACITY)?], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (origin, mutation_id, fingerprint, result_json, revision) in rows.into_iter().rev() {
            let key = format!("{origin}\u{0}{mutation_id}");
            let revision =
                u64::try_from(revision).context("stored mutation revision is negative")?;
            let entry =
                LedgerEntry { fingerprint, result_json, revision, state: EntryState::Committed };
            if cache.entries.insert(key.clone(), entry).is_none() {
                cache.order.push_back(key);
            }
        }
        Ok(cache)
    }

    /// The newest revision committed or admitted.
    fn head(&self) -> u64 {
        self.admitted.last().map_or(self.committed, |admitted| (*admitted).max(self.committed))
    }

    fn replay(&self, request: &LedgerRequest<'_>) -> Result<Option<LedgerReplay>, LedgerRefusal> {
        let Some(entry) = self.entries.get(&ledger_key(request.mutation)) else {
            return Ok(None);
        };
        request.replay_of(&entry.fingerprint, entry.result_json.clone(), entry.revision).map(Some)
    }

    fn admit(&mut self, request: &LedgerRequest<'_>) -> Result<Admission, LedgerRefusal> {
        if let Some(replay) = self.replay(request)? {
            return Ok(Admission::Replay(replay));
        }
        let current = self.head();
        if let Some(expected) = request.expected_revision
            && expected != current
        {
            return Err(LedgerRefusal::RevisionConflict {
                ledger: request.ledger,
                expected,
                current,
            });
        }
        let revision = current
            .checked_add(1)
            .ok_or(LedgerRefusal::RevisionExhausted { ledger: request.ledger })?;
        self.admitted.insert(revision);
        self.entries.insert(
            ledger_key(request.mutation),
            LedgerEntry {
                fingerprint: request.fingerprint.to_string(),
                result_json: request.result_json.to_string(),
                revision,
                state: EntryState::Admitted,
            },
        );
        Ok(Admission::Admitted(revision))
    }

    fn settle(&mut self, key: &str, outcome: LedgerOutcome) {
        match outcome {
            LedgerOutcome::Committed(revision) => {
                if let Some(entry) = self.entries.get_mut(key)
                    && entry.state != EntryState::Committed
                {
                    self.admitted.remove(&entry.revision);
                    entry.revision = revision;
                    entry.state = EntryState::Committed;
                    self.order.push_back(key.to_string());
                }
                self.committed = self.committed.max(revision);
                self.evict();
            }
            LedgerOutcome::Unapplied => {
                if self.entries.get(key).is_some_and(|entry| entry.state != EntryState::Committed)
                    && let Some(entry) = self.entries.remove(key)
                {
                    self.admitted.remove(&entry.revision);
                }
            }
            LedgerOutcome::Indeterminate => {
                if let Some(entry) = self.entries.get_mut(key)
                    && entry.state == EntryState::Admitted
                {
                    entry.state = EntryState::Indeterminate;
                }
            }
        }
    }

    /// Forget the indeterminate entries; true when there were any.
    fn forget_indeterminate(&mut self) -> bool {
        let before = self.entries.len();
        let admitted = &mut self.admitted;
        self.entries.retain(|_, entry| {
            let keep = entry.state != EntryState::Indeterminate;
            if !keep {
                admitted.remove(&entry.revision);
            }
            keep
        });
        self.entries.len() != before
    }

    /// Take the stored revision; a revision that moved back (the registry
    /// was reset) drops every committed entry with it.
    fn resync(&mut self, connection: &Connection, ledger: KeyedLedger) -> anyhow::Result<()> {
        let stored = stored_revision(connection, ledger)?;
        if stored < self.committed {
            self.entries.retain(|_, entry| entry.state != EntryState::Committed);
            self.order.clear();
        }
        self.committed = stored;
        Ok(())
    }

    fn evict(&mut self) {
        while self.order.len() > LEDGER_CACHE_CAPACITY {
            let Some(key) = self.order.pop_front() else { break };
            if self.entries.get(&key).is_some_and(|entry| entry.state == EntryState::Committed) {
                self.entries.remove(&key);
            }
        }
    }
}

fn stored_revision(connection: &Connection, ledger: KeyedLedger) -> anyhow::Result<u64> {
    required_meta(connection, ledger.revision_key())?
        .parse()
        .with_context(|| format!("{} registry revision is invalid", ledger.label()))
}

/// Both ledgers' in-memory revisions and recent entries, owned by the
/// workspace registry (its lock guards them).
#[derive(Debug)]
pub(crate) struct LedgerCache {
    workspace: KeyedCache,
    terminal: KeyedCache,
    /// Set by the connection's update hook (see the module docs).
    stale: Arc<AtomicBool>,
}

impl LedgerCache {
    /// Seed from `connection` and install its update hook. Call last at
    /// open, after every migration write.
    pub(crate) fn seed(connection: &Connection) -> anyhow::Result<Self> {
        let cache = Self {
            workspace: KeyedCache::seed(connection, KeyedLedger::Workspace)?,
            terminal: KeyedCache::seed(connection, KeyedLedger::Terminal)?,
            stale: Arc::new(AtomicBool::new(false)),
        };
        let stale = Arc::clone(&cache.stale);
        connection.update_hook(Some(move |_: Action, _: &str, table: &str, _: i64| {
            if table == "meta" && !ADMITTED_WRITE.with(Cell::get) {
                stale.store(true, Ordering::Release);
            }
        }))?;
        Ok(cache)
    }

    fn keyed(&self, ledger: KeyedLedger) -> &KeyedCache {
        match ledger {
            KeyedLedger::Workspace => &self.workspace,
            KeyedLedger::Terminal => &self.terminal,
        }
    }

    fn keyed_mut(&mut self, ledger: KeyedLedger) -> &mut KeyedCache {
        match ledger {
            KeyedLedger::Workspace => &mut self.workspace,
            KeyedLedger::Terminal => &mut self.terminal,
        }
    }

    /// True when a revision may have moved outside an admitted intent.
    pub(crate) fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Acquire)
    }

    /// Reread both revisions if [`Self::is_stale`].
    pub(crate) fn refresh(&mut self, connection: &Connection) -> anyhow::Result<()> {
        if !self.stale.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let resynced = self
            .workspace
            .resync(connection, KeyedLedger::Workspace)
            .and_then(|()| self.terminal.resync(connection, KeyedLedger::Terminal));
        if resynced.is_err() {
            self.stale.store(true, Ordering::Release);
        }
        resynced
    }

    /// The stored or admitted result of a retried mutation, if cached.
    pub(crate) fn replay(
        &self,
        request: &LedgerRequest<'_>,
    ) -> Result<Option<LedgerReplay>, LedgerRefusal> {
        self.keyed(request.ledger).replay(request)
    }

    /// Replay a cached mutation, or CAS against the newest admitted
    /// revision and admit the intent at the next one.
    pub(crate) fn admit(
        &mut self,
        request: &LedgerRequest<'_>,
    ) -> Result<Admission, LedgerRefusal> {
        self.keyed_mut(request.ledger).admit(request)
    }

    /// Record how an admitted intent's commit answered.
    pub(crate) fn settle(&mut self, ticket: &LedgerTicket, outcome: LedgerOutcome) {
        self.keyed_mut(ticket.ledger).settle(&ticket.key, outcome);
    }

    /// Forget indeterminate intents once their answers are in SQLite (the
    /// caller drained them): the next admission rereads the revisions, and
    /// a retry of one replays from the stored ledger.
    pub(crate) fn reconcile_indeterminate(&mut self) {
        let workspace = self.workspace.forget_indeterminate();
        let terminal = self.terminal.forget_indeterminate();
        if workspace || terminal {
            self.stale.store(true, Ordering::Release);
        }
    }

    /// The newest revision committed or admitted.
    #[cfg(test)]
    pub(crate) fn head(&self, ledger: KeyedLedger) -> u64 {
        self.keyed(ledger).head()
    }

    /// The newest revision known committed.
    #[cfg(test)]
    pub(crate) fn committed(&self, ledger: KeyedLedger) -> u64 {
        self.keyed(ledger).committed
    }
}

#[cfg(test)]
#[path = "mutation_ledger_tests.rs"]
mod tests;
