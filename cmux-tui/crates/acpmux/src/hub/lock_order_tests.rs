//! The hub's and a session's locks carry their ranks (`crate::lock_rank`): the
//! paths that nest them pass, and a lock taken out of order panics in debug
//! and test builds.

use super::*;
use serde_json::json;

fn hub() -> Arc<Hub> {
    let mut cfg = crate::config::Config::default();
    cfg.store.mode = crate::config::StoreMode::Memory;
    let store = crate::store::open(&cfg.store, std::path::Path::new("/nonexistent")).unwrap();
    Hub::new(cfg, store)
}

fn session(hub: &Hub, id: &str) -> Arc<Session> {
    let meta = serde_json::from_value(json!({
        "schema": crate::store::META_SCHEMA,
        "id": id,
        "name": id,
        "harness": "claude",
        "cwd": "/tmp",
        "status": "idle",
        "createdAt": 0,
        "updatedAt": 0,
    }))
    .unwrap();
    let session = hub.make_session(meta);
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
