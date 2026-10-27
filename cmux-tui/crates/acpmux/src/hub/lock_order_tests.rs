//! The hub's and a session's locks carry their ranks (`crate::lock_rank`): the
//! paths that nest them pass, and a lock taken out of order panics in debug
//! and test builds.

use super::*;
use serde_json::{Value, json};

fn hub() -> Arc<Hub> {
    let mut cfg = crate::config::Config::default();
    cfg.store.mode = crate::config::StoreMode::Memory;
    let store = crate::store::open(&cfg.store, std::path::Path::new("/nonexistent")).unwrap();
    Hub::new(cfg, store)
}

fn session(hub: &Hub, id: &str) -> Arc<Session> {
    session_with(hub, id, json!({}))
}

/// A session whose meta also has the fields of `extra`.
fn session_with(hub: &Hub, id: &str, extra: Value) -> Arc<Session> {
    let mut meta = json!({
        "schema": crate::store::META_SCHEMA,
        "id": id,
        "name": id,
        "harness": "claude",
        "cwd": "/tmp",
        "status": "idle",
        "createdAt": 0,
        "updatedAt": 0,
    });
    if let (Some(meta), Value::Object(extra)) = (meta.as_object_mut(), extra) {
        meta.extend(extra);
    }
    let session = hub.make_session(serde_json::from_value(meta).unwrap());
    hub.sessions.lock().unwrap().insert(id.to_owned(), session.clone());
    session
}

/// Records are logged under the queue, the ledger and the permissions (an
/// append takes `append_lock`, `meta` and the clock), as the queue and
/// permission paths do.
#[test]
fn an_append_under_the_session_state_locks_is_in_order() {
    let hub = hub();
    let s = session(&hub, "s1");
    let _queue = s.queue.lock().unwrap();
    let _ledger = s.prompts.lock().unwrap();
    let _permissions = s.permissions.lock().unwrap();
    hub.append(&s, "mux", "status", json!({"status": "idle"}));
    assert_eq!(s.meta().last_seq, 1);
}

/// A lookup reads each session's `meta` under the registry.
#[test]
fn a_session_lookup_under_the_registry_is_in_order() {
    let hub = hub();
    session(&hub, "s1");
    let sessions = hub.sessions.lock().unwrap();
    assert!(sessions.values().all(|s| s.meta().id == "s1"));
}

/// The lease lock is held while the peer and its rows go (`peer_lease.rs`).
#[test]
fn the_peer_locks_nest_lease_rows_peer() {
    let hub = hub();
    let _leases = hub.peer_leases.lock().unwrap();
    let _rows = hub.remote_sessions.lock().unwrap();
    let _peers = hub.peers.lock().unwrap();
}

/// The inversion this ranks against: the registry waited for under a
/// session's `meta`. A thread that holds the registry and reads `meta` (a
/// lookup) and one that holds `meta` and waits for the registry deadlock.
#[test]
#[should_panic(
    expected = "acquiring hub.sessions (Sessions) while this thread holds session.meta (SessionMeta)"
)]
fn the_registry_waited_for_under_a_sessions_meta_panics() {
    let hub = hub();
    let s = session(&hub, "s1");
    let _meta = s.meta.lock().unwrap();
    let _ = hub.sessions();
}

/// `peers` before `peer_leases` is the order `peer_leases` documents against.
#[test]
#[should_panic(
    expected = "acquiring hub.peer_leases (PeerLeases) while this thread holds hub.peers (Peers)"
)]
fn a_lease_waited_for_under_the_peers_panics() {
    let hub = hub();
    let _peers = hub.peers.lock().unwrap();
    let _ = hub.is_leased("p");
}

/// An append under a session's `meta` (its `append_lock` after `meta`).
#[test]
#[should_panic(
    expected = "acquiring session.append_lock (SessionAppend) while this thread holds session.meta"
)]
fn an_append_under_meta_panics() {
    let hub = hub();
    let s = session(&hub, "s1");
    let _meta = s.meta.lock().unwrap();
    hub.append(&s, "mux", "status", json!({"status": "idle"}));
}

/// A closed child, as a kill leaves it while its turn's end waits on the agent.
fn closed_child(hub: &Hub) -> Arc<Session> {
    session(hub, "parent");
    session_with(
        hub,
        "child",
        json!({"status": "closed", "subsession": {"startedBy": "parent", "shortName": "tests", "depth": 1}}),
    )
}

/// A closed sub-session reopened under its `permissions` (a turn's end after a kill, a
/// permission prompt or its answer): the parent's record waits until they are released, as the
/// parent is looked up in the registry, which ranks before every session lock.
#[test]
fn a_reopened_childs_parent_record_waits_for_its_permissions() {
    let hub = hub();
    let child = closed_child(&hub);
    let permissions = child.permissions.lock().unwrap();
    hub.set_status_releasing(&child, SessionStatus::Ready, permissions);
    assert_eq!(child.status(), SessionStatus::Ready);
    let events: Vec<Value> = hub
        .events("parent", 0, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == crate::hub::subsessions::RECORD_KIND)
        .map(|e| e.msg["event"].clone())
        .collect();
    assert_eq!(events, vec![json!("reopened")]);
}

/// The inversion `set_status_releasing` avoids: the plain setter reopening a closed child
/// under its `permissions` waits for the registry.
#[test]
#[should_panic(
    expected = "acquiring hub.sessions (Sessions) while this thread holds session.permissions"
)]
fn a_reopened_childs_parent_looked_up_under_its_permissions_panics() {
    let hub = hub();
    let child = closed_child(&hub);
    let _permissions = child.permissions.lock().unwrap();
    hub.set_status(&child, SessionStatus::Ready);
}

/// The hub's sub-state is taken under any session lock: the secrets under the
/// `permissions` and the turn, the subsession index under `meta`.
#[test]
fn sub_state_under_a_sessions_locks_is_in_order() {
    let hub = hub();
    let s = session(&hub, "s1");
    {
        let _permissions = s.permissions.lock().unwrap();
        let _turn = s.turn.lock().unwrap();
        assert!(!hub.secrets.waiting_in(&s.id));
    }
    let _meta = s.meta.lock().unwrap();
    assert_eq!(hub.subsessions.open_count("s1"), 0);
}

/// A turn's end takes the chat's turn under the auto-resume marks (`take_turn`).
#[test]
fn the_turn_under_the_auto_resume_marks_is_in_order() {
    let hub = hub();
    let s = session(&hub, "s1");
    assert!(hub.take_turn(&s, "t1").is_none());
}

/// The auto-resume marks waited for under a chat's turn: `take_turn` holds the marks and
/// waits for the turn, so a thread holding the turn and waiting for the marks deadlocks it.
#[test]
#[should_panic(
    expected = "acquiring hub.auto_resumes (AutoResumes) while this thread holds session.turn (Leaf)"
)]
fn the_auto_resume_marks_waited_for_under_a_turn_panic() {
    let hub = hub();
    let s = session(&hub, "s1");
    let _turn = s.turn.lock().unwrap();
    let _ = hub.continuation_pending(&s);
}
