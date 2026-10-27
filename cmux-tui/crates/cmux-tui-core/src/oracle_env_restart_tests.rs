//! A seeded owner that restarts on its own state root continues the clock
//! and mints ids no earlier launch minted, so nothing it stored is
//! overwritten; and a read never moves the seeded id stream.

use std::path::Path;
use std::sync::Arc;

use cmux_conversation::Participant;
use serde_json::{Value, json};

use super::FIXTURE_EPOCH_MS;
use super::stores_tests::{StateRoot, mutate};
use crate::Mux;
use crate::conversation_store::LOCAL_USER;
use crate::switches::Switches;

const SESSION: &str = "oracle-env-restart";
const SEED: &str = "restart/case-001";

fn people() -> Vec<Participant> {
    serde_json::from_value(json!([{"id": LOCAL_USER, "kind": "human", "display_name": "Me"}]))
        .unwrap()
}

fn open(root: &Path) -> Arc<Mux> {
    Mux::new_for_test_with_switches(SESSION, Some(root), Switches::seeded(SEED))
}

fn close(mux: Arc<Mux>) {
    mux.shutdown();
    drop(mux);
}

/// One launch's mints: a conversation, a checklist item and the checkpoint
/// store's ids, each under keys of its own `run`.
fn mint_all(mux: &Arc<Mux>, run: &str) -> Value {
    let conversation = mux
        .with_conversations(|store| {
            let created = store.create(&format!("create-{run}"), LOCAL_USER, run, &people())?;
            Ok(json!({"id": created.summary.id, "created_at": created.summary.created_at}))
        })
        .unwrap();
    let workspace = mutate(
        mux,
        "workspace.create",
        json!({"name": run, "initial_content": "empty"}),
        &format!("workspace-{run}"),
    )["workspace_id"]
        .clone();
    let added = mutate(
        mux,
        "workspace_checklist.add",
        json!({"workspace": workspace, "text": run}),
        &format!("add-{run}"),
    );
    json!({
        "conversation": conversation,
        "item": added["items"][0],
        "checkpoints": crate::git_ops::checkpoint_oracle_sample(mux),
    })
}

/// Two launches on one root: the first mints, moves the clock and stops;
/// the second mints again.
fn two_launches(root: &Path) -> (Value, Value, Arc<Mux>) {
    let mux = open(root);
    let first = mint_all(&mux, "first");
    mux.oracle_env().virtual_clock().unwrap().advance(5_000).unwrap();
    close(mux);
    let mux = open(root);
    let second = mint_all(&mux, "second");
    (first, second, mux)
}

#[test]
fn a_seeded_owner_restarted_on_its_root_mints_new_ids_and_keeps_its_clock() {
    let root = StateRoot::new();
    let (first, second, mux) = two_launches(&root.0);
    for path in ["/conversation/id", "/item/id", "/checkpoints/checkpoint"] {
        assert_ne!(first.pointer(path), second.pointer(path), "{path} repeated after the restart");
    }
    assert_eq!(
        first["checkpoints"]["repository"], second["checkpoints"]["repository"],
        "a stored repository id is read back, not minted again"
    );
    let stamped = second["checkpoints"]["now_ms"].as_u64().unwrap();
    assert_eq!(stamped, FIXTURE_EPOCH_MS + 5_000, "the restart resumes the advanced clock");
    assert_eq!(second["item"]["created_at_ms"], stamped.to_string());
    let listed: Vec<String> = mux
        .with_conversations(|store| {
            Ok(store.list()?.into_iter().map(|summary| summary.id).collect())
        })
        .unwrap();
    let ids = [&second["conversation"]["id"], &first["conversation"]["id"]];
    assert_eq!(listed, ids.map(|id| id.as_str().unwrap().to_string()), "both kept, newest first");
    close(mux);

    let again = StateRoot::new();
    let (replay_first, replay_second, mux) = two_launches(&again.0);
    assert_eq!((first, second), (replay_first, replay_second), "one seed, one script, one result");
    close(mux);
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = cmux_process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read(mux: &Arc<Mux>, operation: &str, mut params: Value) -> Value {
    params["machine"] = json!("current");
    params["session"] = json!("current");
    let envelope = json!({
        "protocol": "cmux.protocol/2",
        "type": "request",
        "id": operation,
        "operation": operation,
        "params": params,
    });
    let reply = crate::resource_router::handle_resource_message(mux, &envelope.to_string())
        .unwrap_or_else(|error| panic!("{operation}: {error:?}"));
    assert_eq!(reply["ok"], true, "{operation}: {reply}");
    reply["result"]["value"].clone()
}

/// A checkpoint of a one-commit repository, then, when `live_diff`, a diff
/// of it against the working tree, then a conversation: its id.
fn conversation_after(live_diff: bool) -> String {
    let root = StateRoot::new();
    let repository = root.0.join("repo");
    std::fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(repository.join("a.txt"), "one\n").unwrap();
    git(&repository, &["add", "."]);
    git(
        &repository,
        &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "--quiet", "-m", "one"],
    );
    let state = root.0.join("state");
    let mux = open(&state);
    let path = repository.to_string_lossy().into_owned();
    let created = mutate(&mux, "git.checkpoint.create", json!({"path": path}), "checkpoint");
    if live_diff {
        std::fs::write(repository.join("a.txt"), "two\n").unwrap();
        let from = created["checkpoint_id"].clone();
        read(&mux, "git.checkpoint.diff", json!({"path": path, "from": from}));
    }
    let id = mux
        .with_conversations(|store| {
            Ok(store.create("after", LOCAL_USER, "after", &people())?.summary.id)
        })
        .unwrap();
    close(mux);
    id
}

#[test]
fn a_live_checkpoint_diff_does_not_move_the_seeded_id_stream() {
    assert_eq!(conversation_after(true), conversation_after(false));
}
