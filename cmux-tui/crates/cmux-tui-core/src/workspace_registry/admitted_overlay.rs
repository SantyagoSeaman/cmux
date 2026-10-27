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
//! writer commits in admission order, so SQLite holds it by then.
//!
//! An intent whose topology cannot be derived exactly in memory is cold: an
//! effect or terminal commit, a workspace tombstone (it cascades to screens,
//! panes and tabs in SQL), an intent prepared on a base that is not the
//! newest view, and every intent admitted after one that failed. While any
//! entry is unanswered, a read the overlay does not serve (a cold topology
//! read, or any other first acquisition of the connection) waits for the
//! writer behind the counted read barrier (`read_barrier_waits`); queue Q18
//! moves those reads onto projections.

use std::collections::VecDeque;
use std::panic::Location;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cmux_threads::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel,
};

use super::*;

/// How long the read barrier waits for the writer to answer every admitted
/// intent (the bound of `drain_unsettled`).
const READ_BARRIER_WAIT: Duration = Duration::from_secs(2);

/// What the journal writer did with an admitted intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmittedOutcome {
    Committed,
    /// Rolled back, or its receipt was dropped.
    Failed,
}

/// The writer side of one admitted intent: whoever delivers the writer's
/// receipt answers it (never the request thread that admitted it, which may
/// be the one that waits). Dropping it unanswered counts as a failure.
#[must_use = "an admitted intent the writer never answers keeps the read barrier closed"]
pub(crate) struct AdmittedCommitSignal {
    sender: SyncSender<AdmittedOutcome>,
}

impl AdmittedCommitSignal {
    /// The writer committed the intent (or replayed it: nothing changed).
    pub(crate) fn committed(self) {
        let _ = self.sender.send(AdmittedOutcome::Committed);
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
    /// projection, or a replay of an admitted mutation).
    Unchanged,
    /// Not derivable in memory.
    Cold,
}

struct AdmittedEntry {
    /// `(origin, mutation id)`: a second admission of the same mutation
    /// replays it and changes nothing.
    mutation: Option<(String, String)>,
    /// The topology after this entry (`None`: cold).
    topology: Option<Arc<ResourceTopologySnapshot>>,
    answer: Receiver<AdmittedOutcome>,
    outcome: Option<AdmittedOutcome>,
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

/// The admitted intents of one registry, in admission order.
#[derive(Default)]
pub(crate) struct AdmittedOverlay {
    entries: VecDeque<AdmittedEntry>,
    /// Topology reads the overlay answered without SQLite.
    reads_served: u64,
    /// Acquisitions that waited for the writer behind the barrier.
    read_barrier_waits: u64,
    /// Barrier waits that ended at their bound with an intent unanswered.
    read_barrier_timeouts: u64,
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

    /// Admit one intent the writer will commit after every earlier one.
    /// `base_revision` is the resource revision of the topology the intent
    /// was prepared on (`None`: it read none); a derived topology is kept
    /// only when that base is the newest view.
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
        let topology = match (replay, topology) {
            (true, _) | (false, AdmittedTopology::Unchanged) => unchanged,
            (false, AdmittedTopology::Cold) => None,
            (false, AdmittedTopology::After(after)) => {
                let on_newest = match &view {
                    AdmittedView::Stored => true,
                    AdmittedView::Admitted(newest) => base_revision == Some(newest.revision),
                    AdmittedView::Cold => false,
                };
                on_newest.then_some(after)
            }
        };
        let (sender, answer) = sync_channel(1);
        self.entries.push_back(AdmittedEntry { mutation, topology, answer, outcome: None });
        AdmittedCommitSignal { sender }
    }

    /// Wait (bounded) until the writer answered every admitted intent. True
    /// when this call waited; each wait is a `read_barrier_waits` and a
    /// `journal_wait` (`request_path`) at `site`.
    pub(super) fn barrier(&mut self, site: &'static Location<'static>) -> bool {
        if self.pending() == 0 {
            return false;
        }
        self.read_barrier_waits += 1;
        crate::request_path::note_at(crate::request_path::BlockingKind::JournalWait, site);
        let deadline = Instant::now() + READ_BARRIER_WAIT;
        for entry in self.entries.iter_mut().filter(|entry| entry.outcome.is_none()) {
            let left = deadline.saturating_duration_since(Instant::now());
            // wakeup-allow: deadline one shared bound on waiting for the writer's answer events
            entry.outcome = match entry.answer.recv_timeout(left) {
                Ok(outcome) => Some(outcome),
                Err(RecvTimeoutError::Disconnected) => Some(AdmittedOutcome::Failed),
                Err(RecvTimeoutError::Timeout) => None,
            };
        }
        if self.pending() > 0 {
            self.read_barrier_timeouts += 1;
        }
        true
    }

    /// `(reads served, barrier waits, barrier timeouts)`.
    pub(crate) fn counts(&self) -> (u64, u64, u64) {
        (self.reads_served, self.read_barrier_waits, self.read_barrier_timeouts)
    }

    /// Take the answers that arrived. A failed entry makes it and every
    /// later entry cold (they were derived on its topology); answered
    /// entries at the front leave.
    fn take_answers(&mut self) {
        let mut failed = false;
        for entry in &mut self.entries {
            if entry.outcome.is_none() {
                entry.outcome = match entry.answer.try_recv() {
                    Ok(outcome) => Some(outcome),
                    Err(TryRecvError::Disconnected) => Some(AdmittedOutcome::Failed),
                    Err(TryRecvError::Empty) => None,
                };
            }
            failed |= entry.outcome == Some(AdmittedOutcome::Failed);
            if failed {
                entry.topology = None;
            }
        }
        while self.entries.front().is_some_and(|entry| entry.outcome.is_some()) {
            self.entries.pop_front();
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
}

#[cfg(test)]
#[path = "admitted_overlay_tests.rs"]
mod tests;
