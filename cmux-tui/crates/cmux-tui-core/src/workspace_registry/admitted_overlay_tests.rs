//! The admitted overlay: prepare and hot topology reads see admitted,
//! uncommitted workspace commits without SQLite; cold intents close the
//! counted read barrier until the writer answers them.

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

/// Prepare the create of the last workspace in `workspaces`.
fn prepare_create(
    registry: &WorkspaceRegistry,
    id: &str,
    workspaces: &[RegistryWorkspace],
) -> WorkspaceCommitIntent {
    let created = workspaces.last().unwrap();
    registry
        .prepare_workspace_commit(
            &mutation(id),
            &json!({"op": "create", "key": created.key}),
            None,
            None,
            "workspace-added",
            &created.key,
            workspaces,
            Some(&created.public_id),
            &json!({"key": created.key}),
            true,
        )
        .unwrap()
}

fn live_workspaces(topology: &ResourceTopologySnapshot) -> Vec<WorkspacePublicId> {
    topology.active_screens.iter().map(|(id, _)| id.clone()).collect()
}

/// A stand-in journal writer that holds the connection lock for
/// `WRITER_DELAY` before it commits each intent, then answers its signal.
/// It reports once it holds the lock for the first intent.
fn slow_writer(
    connection: Arc<RegistryConnection>,
) -> (
    SyncSender<(WorkspaceCommitIntent, AdmittedCommitSignal)>,
    Receiver<()>,
    std::thread::JoinHandle<()>,
) {
    let (send, intents) = sync_channel::<(WorkspaceCommitIntent, AdmittedCommitSignal)>(8);
    let (holding, held) = sync_channel(8);
    let writer = std::thread::spawn(move || {
        for (intent, signal) in intents.iter() {
            let db = connection.get_until(Instant::now() + Duration::from_secs(10)).unwrap();
            holding.send(()).unwrap();
            std::thread::sleep(WRITER_DELAY);
            let tx = db.unchecked_transaction().unwrap();
            intent.apply(&tx, None).unwrap();
            tx.commit().unwrap();
            drop(db);
            signal.committed();
        }
    });
    (send, held, writer)
}

#[test]
fn admitted_overlay_two_creates_with_a_slow_writer_prepare_in_memory() {
    let registry = seeded("admitted-two-creates");
    let (send, held, writer) = slow_writer(Arc::clone(&registry.connection));
    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let scope = RequestPathScope::enter(&stats);
    let (one, two, three) = (workspace(1), workspace(2), workspace(3));

    let started = Instant::now();
    let first = prepare_create(&registry, "create-2", &[one.clone(), two.clone()]);
    let first = RegistryIntent::Workspace(first);
    let first_signal = registry.admit_registry_intent(&first);
    let RegistryIntent::Workspace(first) = first else { unreachable!() };
    send.send((first, first_signal)).unwrap();
    let first_create = started.elapsed();

    // The writer now holds the connection lock for WRITER_DELAY: a create
    // that touched SQLite would wait for it.
    held.recv().unwrap();
    let sqlite_before = stats.count(BlockingKind::Sqlite);
    let started = Instant::now();
    let second = prepare_create(&registry, "create-3", &[one.clone(), two.clone(), three.clone()]);
    let (_, second_base, second_topology) = second.admitted();
    let second = RegistryIntent::Workspace(second);
    let second_signal = registry.admit_registry_intent(&second);
    let RegistryIntent::Workspace(second) = second else { unreachable!() };
    send.send((second, second_signal)).unwrap();
    let seen = registry.resource_topology_snapshot().unwrap();
    let second_create = started.elapsed();
    let sqlite_during = stats.count(BlockingKind::Sqlite) - sqlite_before;
    drop(scope);

    assert!(first_create < CREATE_BUDGET, "first create took {first_create:?}");
    assert!(second_create < CREATE_BUDGET, "second create took {second_create:?}");
    assert_eq!(sqlite_during, 0, "the second create read SQLite behind the writer");
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
    let (served, barrier_waits, timeouts) = registry.connection.admitted_counts();
    assert_eq!((barrier_waits, timeouts), (0, 0));
    assert!(served >= 2, "prepare and the hot read were served by the overlay ({served})");
}

#[test]
fn admitted_overlay_cold_intent_waits_behind_the_counted_barrier() {
    let registry = seeded("admitted-cold-barrier");
    let signal = registry.connection.admit_intent(None, None, AdmittedTopology::Cold);
    let answer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        signal.committed();
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
    assert!(matches!(registry.connection.admitted_overlay().view(), AdmittedView::Admitted(_)));

    first_signal.failed();
    assert_eq!(registry.connection.admitted_overlay().view(), AdmittedView::Cold);
    drop(second_signal);
    assert_eq!(registry.connection.admitted_overlay().view(), AdmittedView::Stored);
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

#[test]
fn admitted_overlay_stale_base_is_cold() {
    let registry = seeded("admitted-stale-base");
    let stale = prepare_create(&registry, "stale", &[workspace(1), workspace(3)]);
    let first =
        RegistryIntent::Workspace(prepare_create(&registry, "a", &[workspace(1), workspace(2)]));
    let _first_signal = registry.admit_registry_intent(&first);
    let _stale_signal = registry.admit_registry_intent(&RegistryIntent::Workspace(stale));
    assert_eq!(registry.connection.admitted_overlay().view(), AdmittedView::Cold);
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
    let intent = prepare_create(&registry, "create-2", &workspaces);
    let intent = RegistryIntent::Workspace(intent);
    let signal = registry.admit_registry_intent(&intent);
    let scope = [workspace(2).public_id, workspace(1).public_id];
    let admitted = registry.resource_topology_scoped(&scope).unwrap();

    let RegistryIntent::Workspace(intent) = intent else { unreachable!() };
    {
        let db = registry.connection.get_until(Instant::now() + Duration::from_secs(10)).unwrap();
        let tx = db.unchecked_transaction().unwrap();
        intent.apply(&tx, None).unwrap();
        tx.commit().unwrap();
    }
    signal.committed();
    assert_eq!(registry.resource_topology_scoped(&scope).unwrap(), admitted);
    assert_eq!(registry.connection.admitted_counts().1, 0);
}
