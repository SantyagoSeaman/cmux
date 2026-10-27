//! The admitted overlay: prepare and hot topology reads see admitted,
//! uncommitted workspace commits without SQLite; cold intents close the
//! counted read barrier until the writer answers them, and a read that can
//! fail refuses once the writer is behind.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cmux_threads::sync::mpsc::{Receiver, SyncSender, sync_channel};
use serde_json::json;

use super::*;
use crate::request_path::{BlockingKind, RequestPathMode, RequestPathScope, RequestPathStats};

/// The p05-ipc-133 acceptance bound for one create on the request thread.
const CREATE_BUDGET: Duration = Duration::from_millis(2);
const WRITER_DELAY: Duration = Duration::from_millis(50);

fn workspace(id: u64) -> RegistryWorkspace {
    RegistryWorkspace {
        id,
        public_id: WorkspacePublicId::parse(format!("ws_{id:032x}")).unwrap(),
        key: format!("key-{id}"),
        name: format!("Workspace {id}"),
        name_source: Default::default(),
        name_revision: 0,
        group_key: "default".into(),
    }
}

fn mutation(id: &str) -> WorkspaceMutation {
    WorkspaceMutation::daemon(id.to_string(), "admitted-overlay-test").unwrap()
}

/// A registry whose stored topology has workspace 1, committed locally.
fn seeded(label: &str) -> WorkspaceRegistry {
    let mut registry = WorkspaceRegistry::in_memory(label).unwrap();
    let base = workspace(1);
    registry
        .commit_with_active_workspace(
            &mutation("seed"),
            &json!({"op": "seed"}),
            None,
            None,
            "workspace-added",
            &base.key,
            std::slice::from_ref(&base),
            Some(&base.public_id),
            &json!({"key": base.key}),
        )
        .unwrap();
    registry
}

fn create_fingerprint(created: &RegistryWorkspace) -> Value {
    json!({"op": "create", "key": created.key})
}

/// Prepare the create of the last workspace in `workspaces`.
fn try_prepare_create(
    registry: &WorkspaceRegistry,
    id: &str,
    workspaces: &[RegistryWorkspace],
) -> anyhow::Result<WorkspaceCommitIntent> {
    let created = workspaces.last().unwrap();
    registry.prepare_workspace_commit(
        &mutation(id),
        &create_fingerprint(created),
        None,
        None,
        "workspace-added",
        &created.key,
        workspaces,
        Some(&created.public_id),
        &json!({"key": created.key}),
        true,
    )
}

fn prepare_create(
    registry: &WorkspaceRegistry,
    id: &str,
    workspaces: &[RegistryWorkspace],
) -> WorkspaceCommitIntent {
    try_prepare_create(registry, id, workspaces).unwrap()
}

/// The registry side of a create admitted before its reply, with the
/// connection acquisitions of `Mux::create_empty_workspace_with_mutation_inner`
/// in its order: the replay lookup, the pin before state
/// (`lock_state_pinned`), prepare, admit, then the resource revision for
/// state, all before the pin ends.
fn admit_create(
    registry: &WorkspaceRegistry,
    id: &str,
    workspaces: &[RegistryWorkspace],
) -> (WorkspaceCommitIntent, AdmittedCommitSignal) {
    let created = workspaces.last().unwrap();
    assert!(registry.replay(&mutation(id), &create_fingerprint(created)).unwrap().is_none());
    let pin = registry.connection.pin();
    let intent = RegistryIntent::Workspace(prepare_create(registry, id, workspaces));
    let signal = registry.admit_registry_intent(&intent);
    let RegistryIntent::Workspace(intent) = intent else { unreachable!() };
    let (_, _, topology) = intent.admitted();
    if let AdmittedTopology::After(after) = topology {
        assert_eq!(registry.resource_topology_snapshot().unwrap().revision, after.revision);
    }
    drop(pin);
    (intent, signal)
}

/// Commit `intent` as the journal writer does, and answer its signal.
fn writer_commit(registry: &WorkspaceRegistry, intent: &WorkspaceCommitIntent) -> Option<u64> {
    let db = registry.connection.get_until(Instant::now() + Duration::from_secs(10)).unwrap();
    let tx = db.unchecked_transaction().unwrap();
    let receipt = intent.apply(&tx, None).unwrap();
    tx.commit().unwrap();
    receipt.resource_revision
}

fn live_workspaces(topology: &ResourceTopologySnapshot) -> Vec<WorkspacePublicId> {
    topology.active_screens.iter().map(|(id, _)| id.clone()).collect()
}

fn view(registry: &WorkspaceRegistry) -> AdmittedView {
    registry.connection.admitted_overlay().view()
}

fn newest_revision(registry: &WorkspaceRegistry) -> Option<u64> {
    match view(registry) {
        AdmittedView::Admitted(topology) => Some(topology.revision),
        AdmittedView::Stored | AdmittedView::Cold => None,
    }
}

/// A stand-in journal writer that answers each intent `WRITER_DELAY` after
/// it was sent (a writer busy with an earlier batch), then commits it under
/// the connection lock. It reports when it took the first intent.
fn slow_writer(
    connection: Arc<RegistryConnection>,
) -> (
    SyncSender<(WorkspaceCommitIntent, AdmittedCommitSignal)>,
    Receiver<()>,
    std::thread::JoinHandle<()>,
) {
    let (send, intents) = sync_channel::<(WorkspaceCommitIntent, AdmittedCommitSignal)>(8);
    let (taken, took) = sync_channel(8);
    let writer = std::thread::spawn(move || {
        for (intent, signal) in intents.iter() {
            taken.send(()).unwrap();
            std::thread::sleep(WRITER_DELAY);
            let db = connection.get_until(Instant::now() + Duration::from_secs(10)).unwrap();
            let tx = db.unchecked_transaction().unwrap();
            let receipt = intent.apply(&tx, None).unwrap();
            tx.commit().unwrap();
            drop(db);
            signal.committed(receipt.resource_revision);
        }
    });
    (send, took, writer)
}

#[test]
fn admitted_overlay_two_creates_with_a_slow_writer_prepare_in_memory() {
    let registry = seeded("admitted-two-creates");
    let (send, took, writer) = slow_writer(Arc::clone(&registry.connection));
    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let scope = RequestPathScope::enter(&stats);
    let (one, two, three) = (workspace(1), workspace(2), workspace(3));

    let started = Instant::now();
    let first = admit_create(&registry, "create-2", &[one.clone(), two.clone()]);
    send.send(first).unwrap();
    let first_create = started.elapsed();

    // The writer has the first intent and answers it WRITER_DELAY later.
    took.recv().unwrap();
    let topology_reads = WorkspaceRegistry::full_topology_reads_for_test();
    let started = Instant::now();
    let (second, second_signal) =
        admit_create(&registry, "create-3", &[one.clone(), two.clone(), three.clone()]);
    let (_, second_base, second_topology) = second.admitted();
    send.send((second, second_signal)).unwrap();
    let seen = registry.resource_topology_snapshot().unwrap();
    let second_create = started.elapsed();
    let journal_waits = stats.count(BlockingKind::JournalWait);
    drop(scope);

    assert!(first_create < CREATE_BUDGET, "first create took {first_create:?}");
    assert!(second_create < CREATE_BUDGET, "second create took {second_create:?}");
    assert_eq!(journal_waits, 0, "a create waited for the journal writer");
    assert_eq!(
        WorkspaceRegistry::full_topology_reads_for_test(),
        topology_reads,
        "the second create read the topology from SQLite"
    );
    let AdmittedTopology::After(predicted) = second_topology else {
        panic!("the second create is derivable: {second_topology:?}")
    };
    // The second create saw the first: its base is the first's topology.
    assert_eq!(live_workspaces(&predicted), [one.public_id, two.public_id, three.public_id]);
    assert_eq!(seen, *predicted, "a hot read sees the newest admitted topology");

    drop(send);
    writer.join().unwrap();
    let base = second_base.unwrap();
    let stored = registry.resource_topology_snapshot().unwrap();
    assert_eq!(stored.revision, base + 1);
    assert_eq!(stored, *predicted, "the overlay predicted exactly what the writer committed");
    let (served, barrier_waits, refusals) = registry.connection.admitted_counts();
    assert_eq!((barrier_waits, refusals), (0, 0));
    assert!(served >= 2, "prepare and the hot read were served by the overlay ({served})");
}

#[test]
fn admitted_overlay_cold_intent_waits_behind_the_counted_barrier() {
    let registry = seeded("admitted-cold-barrier");
    let signal = registry.connection.admit_intent(None, None, AdmittedTopology::Cold);
    let answer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        signal.committed(None);
    });
    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let scope = RequestPathScope::enter(&stats);
    let topology = registry.resource_topology_snapshot().unwrap();
    drop(scope);
    answer.join().unwrap();
    assert_eq!(live_workspaces(&topology), [workspace(1).public_id]);
    assert_eq!(registry.connection.admitted_counts(), (0, 1, 0));
    assert_eq!(stats.count(BlockingKind::JournalWait), 1);
    // Answered: the next read neither waits nor counts.
    registry.resource_topology_snapshot().unwrap();
    assert_eq!(registry.connection.admitted_counts(), (0, 1, 0));
}

#[test]
fn admitted_overlay_failure_makes_later_entries_cold() {
    let registry = seeded("admitted-failure");
    let (one, two, three) = (workspace(1), workspace(2), workspace(3));
    let first =
        RegistryIntent::Workspace(prepare_create(&registry, "a", &[one.clone(), two.clone()]));
    let first_signal = registry.admit_registry_intent(&first);
    let second = RegistryIntent::Workspace(prepare_create(&registry, "b", &[one, two, three]));
    let second_signal = registry.admit_registry_intent(&second);
    assert!(matches!(view(&registry), AdmittedView::Admitted(_)));

    first_signal.failed();
    assert_eq!(view(&registry), AdmittedView::Cold);
    drop(second_signal);
    assert_eq!(view(&registry), AdmittedView::Stored);
}

/// Prepared on the first intent's topology, admitted after the first
/// failed: SQLite never holds the first, so the second is not derived.
#[test]
fn admitted_overlay_base_of_a_failed_intent_is_cold() {
    let registry = seeded("admitted-failed-base");
    let stored = registry.resource_topology_snapshot().unwrap().revision;
    let (one, two, three) = (workspace(1), workspace(2), workspace(3));
    let (_, first_signal) = admit_create(&registry, "a", &[one.clone(), two.clone()]);
    let second = prepare_create(&registry, "b", &[one, two, three]);
    assert_eq!(second.admitted().1, Some(stored + 1), "prepared on the admitted topology");

    first_signal.failed();
    let _second_signal = registry.admit_registry_intent(&RegistryIntent::Workspace(second));
    assert_eq!(view(&registry), AdmittedView::Cold);
}

/// Prepared on the first intent's topology, admitted after the first
/// committed and left: SQLite holds that base, so the second stays hot.
#[test]
fn admitted_overlay_base_of_a_committed_intent_stays_hot() {
    let registry = seeded("admitted-committed-base");
    let (one, two, three) = (workspace(1), workspace(2), workspace(3));
    let (first, first_signal) = admit_create(&registry, "a", &[one.clone(), two.clone()]);
    let second = prepare_create(&registry, "b", &[one, two, three]);
    let (_, _, AdmittedTopology::After(predicted)) = second.admitted() else {
        panic!("the second create is derivable")
    };

    first_signal.committed(writer_commit(&registry, &first));
    let second_signal = registry.admit_registry_intent(&RegistryIntent::Workspace(second));
    assert_eq!(view(&registry), AdmittedView::Admitted(Arc::clone(&predicted)));
    drop(second_signal);
}

/// The writer committed something other than the prediction (here: it
/// replayed): the entries derived on it go cold.
#[test]
fn admitted_overlay_mispredicted_commit_makes_later_entries_cold() {
    let registry = seeded("admitted-mispredicted");
    let (one, two, three) = (workspace(1), workspace(2), workspace(3));
    let (_, first_signal) = admit_create(&registry, "a", &[one.clone(), two.clone()]);
    let (_, _second_signal) = admit_create(&registry, "b", &[one, two, three]);
    assert!(matches!(view(&registry), AdmittedView::Admitted(_)));
    first_signal.committed(None);
    assert_eq!(view(&registry), AdmittedView::Cold);
}

#[test]
fn admitted_overlay_replayed_mutation_changes_nothing() {
    let registry = seeded("admitted-replay");
    let workspaces = [workspace(1), workspace(2)];
    let first = RegistryIntent::Workspace(prepare_create(&registry, "same", &workspaces));
    let _first_signal = registry.admit_registry_intent(&first);
    let after_first = registry.resource_topology_snapshot().unwrap();
    let again = RegistryIntent::Workspace(prepare_create(&registry, "same", &workspaces));
    let _again_signal = registry.admit_registry_intent(&again);
    assert_eq!(registry.resource_topology_snapshot().unwrap(), after_first);
}

/// A retry of a mutation the writer already committed replays: neither the
/// overlay nor SQLite moves past the committed revision.
#[test]
fn admitted_overlay_retry_after_commit_changes_nothing() {
    let registry = seeded("admitted-retry-after-commit");
    let workspaces = [workspace(1), workspace(2)];
    let (first, first_signal) = admit_create(&registry, "same", &workspaces);
    first_signal.committed(writer_commit(&registry, &first));
    let committed = registry.resource_topology_snapshot().unwrap();

    let retry = RegistryIntent::Workspace(prepare_create(&registry, "same", &workspaces));
    let retry_signal = registry.admit_registry_intent(&retry);
    assert_eq!(view(&registry), AdmittedView::Cold, "a replay derives no topology");
    let RegistryIntent::Workspace(retry) = retry else { unreachable!() };
    let installed = writer_commit(&registry, &retry);
    assert_eq!(installed, None, "the writer replays it");
    retry_signal.committed(installed);
    assert_eq!(registry.resource_topology_snapshot().unwrap(), committed);

    // The same retry while another create is admitted: the newest revision
    // is that create's, not one past it.
    let (_, _other_signal) =
        admit_create(&registry, "other", &[workspace(1), workspace(2), workspace(3)]);
    let newest = newest_revision(&registry);
    assert_eq!(newest, Some(committed.revision + 1));
    let retry = RegistryIntent::Workspace(prepare_create(&registry, "same", &workspaces));
    let _retry_signal = registry.admit_registry_intent(&retry);
    assert_eq!(newest_revision(&registry), newest);
}

#[test]
fn admitted_overlay_stale_base_is_cold() {
    let registry = seeded("admitted-stale-base");
    let stale = prepare_create(&registry, "stale", &[workspace(1), workspace(3)]);
    let first =
        RegistryIntent::Workspace(prepare_create(&registry, "a", &[workspace(1), workspace(2)]));
    let _first_signal = registry.admit_registry_intent(&first);
    let _stale_signal = registry.admit_registry_intent(&RegistryIntent::Workspace(stale));
    assert_eq!(view(&registry), AdmittedView::Cold);
}

/// The replay lookup of a mutation no admitted intent carries skips the
/// barrier; one an unanswered intent carries reads behind it.
#[test]
fn admitted_overlay_replay_lookup_waits_only_for_its_own_mutation() {
    let registry = seeded("admitted-replay-lookup");
    let created = workspace(2);
    let (_, _signal) = admit_create(&registry, "a", &[workspace(1), created.clone()]);
    registry.connection.admitted_overlay().set_barrier_bound(Duration::from_millis(10));
    assert!(registry.replay(&mutation("b"), &create_fingerprint(&created)).unwrap().is_none());
    assert_eq!(registry.connection.admitted_counts().1, 0);
    let refused = registry.replay(&mutation("a"), &create_fingerprint(&created)).unwrap_err();
    assert!(refused.downcast_ref::<AdmittedJournalBehind>().is_some(), "{refused:#}");
}

/// A writer that never answers: reads and writes that can fail refuse with
/// a typed error (once after the bound, then at once) and never see the
/// stored state without the admitted intent; a plain acquisition waits for
/// the writer's answer.
#[test]
fn admitted_overlay_unanswered_intent_refuses_reads_and_local_writes() {
    const BOUND: Duration = Duration::from_millis(20);
    let registry = seeded("admitted-unanswered");
    let signal = registry.connection.admit_intent(None, None, AdmittedTopology::Cold);
    registry.connection.admitted_overlay().set_barrier_bound(BOUND);
    let behind = |error: anyhow::Error| {
        assert!(error.downcast_ref::<AdmittedJournalBehind>().is_some(), "{error:#}");
    };

    let started = Instant::now();
    behind(registry.resource_topology_snapshot().unwrap_err());
    assert!(started.elapsed() >= BOUND, "the first refusal waits its bound");
    let started = Instant::now();
    behind(registry.resource_topology_snapshot().unwrap_err());
    behind(registry.resource_topology_scoped(&[workspace(1).public_id]).unwrap_err());
    behind(try_prepare_create(&registry, "a", &[workspace(1), workspace(2)]).unwrap_err());
    assert!(started.elapsed() < BOUND, "later refusals do not wait again");
    assert_eq!(registry.connection.admitted_counts(), (0, 1, 4));

    let local = RegistryIntent::Workspace(
        registry
            .prepare_workspace_commit(
                &mutation("local"),
                &json!({"op": "local"}),
                None,
                None,
                "workspace-added",
                "key-1",
                &[workspace(1)],
                None,
                &json!({}),
                false,
            )
            .unwrap(),
    );
    behind(registry.commit_registry_intent_locally(&local).unwrap_err());

    let answer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        signal.failed();
    });
    drop(registry.connection.get());
    answer.join().unwrap();
    assert_eq!(registry.connection.admitted_overlay().pending(), 0);
    assert_eq!(
        live_workspaces(&registry.resource_topology_snapshot().unwrap()),
        [workspace(1).public_id]
    );
}

#[test]
fn admitted_overlay_tombstone_is_not_derived() {
    let registry = seeded("admitted-tombstone");
    let previous = registry.resource_topology_snapshot().unwrap();
    let replacement = workspace(2);
    assert!(
        topology_after_workspace_commit(
            &previous,
            std::slice::from_ref(&replacement),
            Some(&replacement.public_id)
        )
        .is_none()
    );
}

#[test]
fn admitted_overlay_scoped_read_matches_the_stored_scope() {
    let registry = seeded("admitted-scoped");
    let workspaces = [workspace(1), workspace(2)];
    let (intent, signal) = admit_create(&registry, "create-2", &workspaces);
    let scope = [workspace(2).public_id, workspace(1).public_id];
    let admitted = registry.resource_topology_scoped(&scope).unwrap();

    signal.committed(writer_commit(&registry, &intent));
    assert_eq!(registry.resource_topology_scoped(&scope).unwrap(), admitted);
    assert_eq!(registry.connection.admitted_counts().1, 0);
}

/// A pin while every admitted intent is on the overlay does not wait for
/// the writer; a local write under it refuses at once instead of
/// committing ahead of the admitted intents.
#[test]
fn admitted_overlay_hot_pin_does_not_wait_and_refuses_local_writes() {
    let registry = seeded("admitted-hot-pin");
    let (_, signal) = admit_create(&registry, "a", &[workspace(1), workspace(2)]);
    let started = Instant::now();
    let pin = registry.connection.pin();
    let local = RegistryIntent::Workspace(
        registry
            .prepare_workspace_commit(
                &mutation("local"),
                &json!({"op": "local"}),
                None,
                None,
                "workspace-added",
                "key-1",
                &[workspace(1)],
                None,
                &json!({}),
                false,
            )
            .unwrap(),
    );
    let refused = registry.commit_registry_intent_locally(&local).unwrap_err();
    assert!(refused.downcast_ref::<AdmittedJournalBehind>().is_some(), "{refused:#}");
    assert!(started.elapsed() < CREATE_BUDGET, "the pin waited {:?}", started.elapsed());
    drop(pin);
    assert_eq!(registry.connection.admitted_counts().1, 0);
    drop(signal);
}
