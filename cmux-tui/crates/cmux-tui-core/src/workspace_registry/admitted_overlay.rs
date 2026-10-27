//! Admitted, uncommitted resource topology (F10, p05-ipc.md section 3.6).
//!
//! Once a request replies at admit (p05-ipc-120), the journal writer commits
//! its intent after the reply. The next request's prepare must build on the
//! topology the admitted intents install, and must not wait for the writer
//! to read it. The overlay keeps, per admitted intent in admission order, the
//! full resource topology after it; prepare and the hot topology reads use
//! the newest one before SQLite. This is LevelDB's read order (Chromium's
//! third_party/leveldatabase/src/db/db_impl.cc, `DBImpl::Get`: the memtable,
//! then the immutable memtable, then the table files).
//!
//! An entry leaves once the writer answered it and every earlier entry: the
//! writer commits in admission order, so SQLite holds it by then. The
//! overlay keeps the resource revision SQLite holds as far as it knows it
//! (`stored`): an intent prepared on the stored topology is derived only
//! when its base is that revision.
//!
//! An intent whose topology cannot be derived exactly in memory is cold: an
//! effect or terminal commit, a workspace tombstone (it cascades to screens,
//! panes and tabs in SQL), an intent prepared on a base that is not the
//! newest view, and every intent after one the writer did not commit as
//! predicted (rolled back, replayed, or at another revision). A replay of a
//! committed or admitted mutation changes nothing.
//!
//! A read the overlay does not serve passes the read barrier first: it waits
//! for the writer to answer every admitted intent (`read_barrier_waits`).
//! A read that can fail waits once, up to its bound, then refuses with
//! [`AdmittedJournalBehind`] (journal degraded, retry), and refuses at once
//! while an entry it already waited for is still unanswered. A committed
//! `mutations` row that no entry carries is final, so its lookup skips the
//! barrier. A pin (`RegistryConnection::pin`) waits only for cold entries:
//! when every entry is on the overlay, the flow under it reads from the
//! overlay, and a read under it that needs SQLite refuses. Queue Q18 moves
//! the other reads onto projections.

use std::collections::VecDeque;
use std::panic::Location;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cmux_threads::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel,
};

use super::*;

/// How long a read that can fail waits for the writer to answer every
/// admitted intent before it refuses (the bound of `drain_unsettled`).
const READ_BARRIER_WAIT: Duration = Duration::from_secs(2);

/// A read that needs SQLite to hold every admitted intent, refused because
/// the journal writer did not answer them within the read barrier's bound.
/// Retry once the writer catches up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdmittedJournalBehind {
    /// Admitted intents the writer has not answered.
    pub(crate) unanswered: usize,
}

impl std::fmt::Display for AdmittedJournalBehind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the session journal writer has not committed {} admitted registry intent(s) \
             (journal degraded); retry",
            self.unanswered
        )
    }
}

impl std::error::Error for AdmittedJournalBehind {}

/// What the journal writer did with an admitted intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmittedOutcome {
    /// Committed; `Some(revision)` is the resource revision it installed,
    /// `None` when it left the resource topology as it was.
    Committed(Option<u64>),
    /// Rolled back: nothing changed.
    Failed,
    /// The receipt was dropped unanswered: whether it committed is unknown.
    Dropped,
}

/// The writer side of one admitted intent: whoever delivers the writer's
/// receipt answers it (never the request thread that admitted it, which may
/// be the one that waits). Dropping it unanswered counts as indeterminate.
#[must_use = "an admitted intent the writer never answers keeps the read barrier closed"]
pub(crate) struct AdmittedCommitSignal {
    sender: SyncSender<AdmittedOutcome>,
}

impl AdmittedCommitSignal {
    /// The writer committed the intent. `resource_revision` is the resource
    /// revision the commit installed (`WorkspaceCommitReceipt`), `None` when
    /// it left the resource topology as it was (a replay, or a commit
    /// without a resource projection).
    pub(crate) fn committed(self, resource_revision: Option<u64>) {
        let _ = self.sender.send(AdmittedOutcome::Committed(resource_revision));
    }

    /// The writer rolled the intent back.
    pub(crate) fn failed(self) {
        let _ = self.sender.send(AdmittedOutcome::Failed);
    }
}

/// The resource topology an admitted intent leaves.
#[derive(Debug, Clone)]
pub(super) enum AdmittedTopology {
    /// Exactly this, once the writer commits it.
    After(Arc<ResourceTopologySnapshot>),
    /// The topology is not changed (a workspace commit without a resource
    /// projection, or a replay of a committed or admitted mutation).
    Unchanged,
    /// Not derivable in memory.
    Cold,
}

/// What an entry does to the stored resource revision once committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoredChange {
    /// It installs this revision.
    Installs(u64),
    /// It leaves the resource topology as it is.
    Keeps,
    /// Not known in memory.
    Unknown,
}

impl StoredChange {
    /// Whether a commit that installed `resource_revision` is the one the
    /// overlay predicted.
    fn predicted(self, resource_revision: Option<u64>) -> bool {
        match self {
            Self::Installs(revision) => resource_revision == Some(revision),
            Self::Keeps => resource_revision.is_none(),
            Self::Unknown => true,
        }
    }
}

struct AdmittedEntry {
    /// `(origin, mutation id)`; `None` when the intent may carry a mutation
    /// the overlay does not know (an effect or terminal commit).
    mutation: Option<(String, String)>,
    change: StoredChange,
    /// The topology after this entry (`None`: cold).
    topology: Option<Arc<ResourceTopologySnapshot>>,
    answer: Receiver<AdmittedOutcome>,
    outcome: Option<AdmittedOutcome>,
    /// A read that can fail already waited its full bound for this entry.
    overdue: bool,
}

/// What a topology read sees now.
#[derive(Debug, PartialEq)]
pub(super) enum AdmittedView {
    /// Nothing is admitted and unanswered: SQLite is current.
    Stored,
    /// The topology after every admitted intent.
    Admitted(Arc<ResourceTopologySnapshot>),
    /// An admitted intent is cold: read SQLite after the barrier.
    Cold,
}

/// Whether an unanswered admitted intent carries a mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MutationAdmission {
    /// An entry carries it.
    Admitted,
    /// No entry carries it: its committed `mutations` row is final.
    NotAdmitted,
    /// No known entry carries it, but an entry's mutation is not known.
    Unknown,
}

/// The admitted intents of one registry, in admission order.
pub(crate) struct AdmittedOverlay {
    entries: VecDeque<AdmittedEntry>,
    /// The resource revision SQLite holds, when the overlay knows it: from
    /// a topology read while nothing was admitted, then from each entry
    /// that leaves.
    stored: Option<u64>,
    /// How long a read that can fail waits for the writer.
    barrier_bound: Duration,
    /// Topology reads the overlay answered without SQLite.
    reads_served: u64,
    /// Acquisitions that waited for the writer behind the barrier.
    read_barrier_waits: u64,
    /// Reads refused with an intent unanswered.
    read_barrier_timeouts: u64,
}

impl Default for AdmittedOverlay {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            stored: None,
            barrier_bound: READ_BARRIER_WAIT,
            reads_served: 0,
            read_barrier_waits: 0,
            read_barrier_timeouts: 0,
        }
    }
}

impl AdmittedOverlay {
    /// Admitted intents the writer has not answered, or that wait for an
    /// earlier one, after taking every answer that arrived.
    pub(super) fn pending(&mut self) -> usize {
        self.take_answers();
        self.entries.len()
    }

    /// The topology a read sees now.
    pub(super) fn view(&mut self) -> AdmittedView {
        self.take_answers();
        match self.entries.back() {
            None => AdmittedView::Stored,
            Some(AdmittedEntry { topology: Some(topology), .. }) => {
                AdmittedView::Admitted(Arc::clone(topology))
            }
            Some(_) => AdmittedView::Cold,
        }
    }

    /// The newest admitted topology for a hot read, counted; `None` when the
    /// read goes to SQLite.
    pub(super) fn serve(&mut self) -> Option<Arc<ResourceTopologySnapshot>> {
        match self.view() {
            AdmittedView::Admitted(topology) => {
                self.reads_served += 1;
                Some(topology)
            }
            AdmittedView::Stored | AdmittedView::Cold => None,
        }
    }

    /// A topology read from SQLite at `revision`: the stored revision, when
    /// nothing is admitted (else SQLite may not hold every entry yet).
    pub(super) fn note_stored(&mut self, revision: u64) {
        if self.pending() == 0 {
            self.stored = Some(revision);
        }
    }

    /// Whether an unanswered entry carries `mutation`.
    pub(super) fn admission(&mut self, mutation: &(String, String)) -> MutationAdmission {
        self.take_answers();
        let mut admission = MutationAdmission::NotAdmitted;
        for entry in &self.entries {
            match &entry.mutation {
                Some(carried) if carried == mutation => return MutationAdmission::Admitted,
                Some(_) => {}
                None => admission = MutationAdmission::Unknown,
            }
        }
        admission
    }

    /// Admit one intent the writer will commit after every earlier one.
    /// `base_revision` is the resource revision of the topology the intent
    /// was prepared on (`None`: it read none); a derived topology is kept
    /// only when that base is the newest view: the newest admitted topology,
    /// or the stored revision when nothing is admitted.
    pub(super) fn admit(
        &mut self,
        mutation: Option<(String, String)>,
        base_revision: Option<u64>,
        topology: AdmittedTopology,
    ) -> AdmittedCommitSignal {
        let view = self.view();
        let replay = mutation.as_ref().is_some_and(|mutation| {
            self.entries.iter().any(|entry| entry.mutation.as_ref() == Some(mutation))
        });
        let unchanged = match &view {
            AdmittedView::Admitted(newest) => Some(Arc::clone(newest)),
            AdmittedView::Stored | AdmittedView::Cold => None,
        };
        let (change, topology) = match (replay, topology) {
            (true, _) | (false, AdmittedTopology::Unchanged) => (StoredChange::Keeps, unchanged),
            (false, AdmittedTopology::Cold) => (StoredChange::Unknown, None),
            (false, AdmittedTopology::After(after)) => {
                let newest = match &view {
                    AdmittedView::Stored => self.stored,
                    AdmittedView::Admitted(newest) => Some(newest.revision),
                    AdmittedView::Cold => None,
                };
                if base_revision.is_some() && base_revision == newest {
                    (StoredChange::Installs(after.revision), Some(after))
                } else {
                    (StoredChange::Unknown, None)
                }
            }
        };
        let (sender, answer) = sync_channel(1);
        self.entries.push_back(AdmittedEntry {
            mutation,
            change,
            topology,
            answer,
            outcome: None,
            overdue: false,
        });
        AdmittedCommitSignal { sender }
    }

    /// The barrier of a read that can fail: wait up to the bound for the
    /// writer to answer every admitted intent, never again for an entry an
    /// earlier read already waited the full bound for. `Ok(true)` when this
    /// call waited; each wait is a `read_barrier_waits` and a `journal_wait`
    /// (`request_path`) at `site`.
    pub(super) fn barrier(
        &mut self,
        site: &'static Location<'static>,
    ) -> Result<bool, AdmittedJournalBehind> {
        if self.pending() == 0 {
            return Ok(false);
        }
        let waits = self.entries.iter().any(|entry| entry.outcome.is_none() && !entry.overdue);
        if waits {
            self.read_barrier_waits += 1;
            crate::request_path::note_at(crate::request_path::BlockingKind::JournalWait, site);
            let deadline = Instant::now() + self.barrier_bound;
            for entry in
                self.entries.iter_mut().filter(|entry| entry.outcome.is_none() && !entry.overdue)
            {
                let left = deadline.saturating_duration_since(Instant::now());
                // wakeup-allow: deadline one shared bound on waiting for the writer's answer events
                entry.outcome = match entry.answer.recv_timeout(left) {
                    Ok(outcome) => Some(outcome),
                    Err(RecvTimeoutError::Disconnected) => Some(AdmittedOutcome::Dropped),
                    Err(RecvTimeoutError::Timeout) => None,
                };
            }
        }
        let unanswered = self.pending();
        if unanswered == 0 {
            return Ok(waits);
        }
        for entry in self.entries.iter_mut().filter(|entry| entry.outcome.is_none()) {
            entry.overdue = true;
        }
        self.read_barrier_timeouts += 1;
        Err(AdmittedJournalBehind { unanswered })
    }

    /// The barrier of a plain connection acquisition, which cannot fail:
    /// wait until the writer answered every admitted intent, as the
    /// acquisition waits for the lock the writer commits under. The writer
    /// answers each intent: it commits it, rolls it back, or drops it.
    pub(super) fn wait_answered(&mut self, site: &'static Location<'static>) {
        if self.pending() == 0 {
            return;
        }
        self.read_barrier_waits += 1;
        crate::request_path::note_at(crate::request_path::BlockingKind::JournalWait, site);
        for entry in self.entries.iter_mut().filter(|entry| entry.outcome.is_none()) {
            entry.outcome = Some(entry.answer.recv().unwrap_or(AdmittedOutcome::Dropped));
        }
        self.take_answers();
    }

    /// The barrier of a pin: wait for the writer's answers only when an
    /// entry is cold. When every entry is on the overlay, the pin is taken
    /// ahead of SQLite.
    pub(super) fn wait_unless_hot(&mut self, site: &'static Location<'static>) {
        if self.view() == AdmittedView::Cold {
            self.wait_answered(site);
        }
    }

    /// A read that needs SQLite under a hold taken ahead of it: refused
    /// while an entry is unanswered (it cannot wait: the writer needs the
    /// lock the hold keeps).
    pub(super) fn refuse_ahead(&mut self) -> Result<(), AdmittedJournalBehind> {
        let unanswered = self.pending();
        if unanswered == 0 {
            return Ok(());
        }
        self.read_barrier_timeouts += 1;
        Err(AdmittedJournalBehind { unanswered })
    }

    /// `(reads served, barrier waits, barrier refusals)`.
    pub(crate) fn counts(&self) -> (u64, u64, u64) {
        (self.reads_served, self.read_barrier_waits, self.read_barrier_timeouts)
    }

    /// Bound the barrier of reads that can fail (tests).
    #[cfg(test)]
    pub(super) fn set_barrier_bound(&mut self, bound: Duration) {
        self.barrier_bound = bound;
    }

    /// Take the answers that arrived. An entry the writer did not commit as
    /// predicted makes it and every later entry cold (they were derived on
    /// its topology); answered entries at the front leave and update the
    /// stored revision.
    fn take_answers(&mut self) {
        let mut mispredicted = false;
        for entry in &mut self.entries {
            if entry.outcome.is_none() {
                entry.outcome = match entry.answer.try_recv() {
                    Ok(outcome) => Some(outcome),
                    Err(TryRecvError::Disconnected) => Some(AdmittedOutcome::Dropped),
                    Err(TryRecvError::Empty) => None,
                };
            }
            mispredicted |= match entry.outcome {
                Some(AdmittedOutcome::Committed(revision)) => !entry.change.predicted(revision),
                Some(AdmittedOutcome::Failed | AdmittedOutcome::Dropped) => true,
                None => false,
            };
            if mispredicted {
                entry.topology = None;
                entry.change = StoredChange::Unknown;
            }
        }
        while let Some(outcome) = self.entries.front().and_then(|entry| entry.outcome) {
            let Some(entry) = self.entries.pop_front() else { break };
            self.stored = match (outcome, entry.change) {
                (AdmittedOutcome::Committed(Some(revision)), _) => Some(revision),
                (AdmittedOutcome::Committed(None), StoredChange::Keeps)
                | (AdmittedOutcome::Failed, _) => self.stored,
                (AdmittedOutcome::Committed(None), _) | (AdmittedOutcome::Dropped, _) => None,
            };
        }
    }
}

/// The resource topology after a workspace registry commit with a resource
/// projection on `previous`, as `WorkspaceCommitIntent::apply` writes it;
/// `None` when it tombstones a workspace (the cascade runs in SQL).
///
/// `load_topology_frame` lists live workspaces by creation revision, then
/// public id: the commit keeps every existing one in place and appends the
/// ones it creates (all at the new revision) by public id, with no active
/// screen. Screens, panes, tabs and browsers do not change.
pub(super) fn topology_after_workspace_commit(
    previous: &ResourceTopologySnapshot,
    workspaces: &[RegistryWorkspace],
    active_workspace: Option<&WorkspacePublicId>,
) -> Option<ResourceTopologySnapshot> {
    let live = workspaces.iter().map(|workspace| &workspace.public_id).collect::<HashSet<_>>();
    if previous.active_screens.iter().any(|(workspace_id, _)| !live.contains(workspace_id)) {
        return None;
    }
    let known = previous.active_screens.iter().map(|(id, _)| id).collect::<HashSet<_>>();
    let mut created = workspaces
        .iter()
        .map(|workspace| &workspace.public_id)
        .filter(|id| !known.contains(id))
        .collect::<Vec<_>>();
    created.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    created.dedup();
    let mut topology = previous.clone();
    topology.revision = previous.revision.checked_add(1)?;
    topology.active_workspace = active_workspace.cloned();
    topology.active_screens.extend(created.into_iter().map(|id| (id.clone(), None)));
    Some(topology)
}

/// The subtrees of `workspaces` in `topology`, in the order
/// `load_resource_topology_scoped` reads them from SQLite.
pub(super) fn scoped(
    topology: &ResourceTopologySnapshot,
    workspaces: &[WorkspacePublicId],
) -> ResourceTopologySnapshot {
    let mut scoped = ResourceTopologySnapshot {
        session_id: topology.session_id.clone(),
        generation: topology.generation.clone(),
        revision: topology.revision,
        active_workspace: topology.active_workspace.clone(),
        active_screens: topology.active_screens.clone(),
        screens: Vec::new(),
        panes: Vec::new(),
        tabs: Vec::new(),
        browsers: Vec::new(),
    };
    for workspace in workspaces {
        scoped.screens.extend(
            topology.screens.iter().filter(|screen| &screen.workspace_id == workspace).cloned(),
        );
    }
    for screen in &scoped.screens {
        scoped.panes.extend(
            topology.panes.iter().filter(|pane| pane.screen_id == screen.public_id).cloned(),
        );
    }
    for pane in &scoped.panes {
        scoped
            .tabs
            .extend(topology.tabs.iter().filter(|tab| tab.pane_id == pane.public_id).cloned());
    }
    for tab in &scoped.tabs {
        if let ContentPublicId::Browser(browser) = &tab.content_id {
            scoped.browsers.extend(
                topology.browsers.iter().filter(|shown| &shown.public_id == browser).cloned(),
            );
        }
    }
    scoped
}

/// A committed `mutations` row: fingerprint, result JSON, revision.
pub(super) type CommittedMutationRow = (String, String, i64);

impl WorkspaceRegistry {
    /// Admit `intent` to the overlay before it goes to the journal writer
    /// (p05-ipc-120 replies after this). The caller holds the registry from
    /// the intent's prepare to here, and hands the signal to whoever
    /// delivers the writer's receipt.
    pub(crate) fn admit_registry_intent(&self, intent: &RegistryIntent) -> AdmittedCommitSignal {
        let (mutation, base_revision, topology) = match intent {
            RegistryIntent::Workspace(intent) => intent.admitted(),
            RegistryIntent::Effect(_) | RegistryIntent::Terminal(_) => {
                (None, None, AdmittedTopology::Cold)
            }
        };
        self.connection.admit_intent(mutation, base_revision, topology)
    }

    /// The committed `mutations` row of `mutation`. Admission needs the
    /// registry, which the caller holds, so no entry can start carrying it
    /// meanwhile: when none does, the stored row is final and the lookup
    /// skips the read barrier; otherwise it reads after the barrier.
    #[track_caller]
    pub(super) fn committed_mutation_row(
        &self,
        mutation: &WorkspaceMutation,
    ) -> anyhow::Result<Option<CommittedMutationRow>> {
        let site = Location::caller();
        let key = (mutation.origin.clone(), mutation.id.clone());
        let db = match self.connection.mutation_admission(&key) {
            MutationAdmission::NotAdmitted => self.connection.get_unbarriered_at(site),
            MutationAdmission::Admitted | MutationAdmission::Unknown => {
                self.connection.get_checked_at(site)?
            }
        };
        Ok(db
            .query_row(
                "SELECT fingerprint, result_json, committed_revision
                 FROM mutations WHERE origin = ?1 AND mutation_id = ?2",
                params![mutation.origin, mutation.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    /// Whether committing `mutation` replays it: an admitted intent carries
    /// it, or its `mutations` row is committed. A replay changes nothing.
    #[track_caller]
    pub(super) fn mutation_replays(&self, mutation: &WorkspaceMutation) -> anyhow::Result<bool> {
        let key = (mutation.origin.clone(), mutation.id.clone());
        if self.connection.mutation_admission(&key) == MutationAdmission::Admitted {
            return Ok(true);
        }
        Ok(self.committed_mutation_row(mutation)?.is_some())
    }
}

#[cfg(test)]
#[path = "admitted_overlay_tests.rs"]
mod tests;
