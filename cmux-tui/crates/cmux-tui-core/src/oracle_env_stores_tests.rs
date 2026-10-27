//! The converted stores (conversations, workspace checklists, checkpoint
//! ids) under one `CMUX_TUI_ORACLE_SEED` write the same bytes: from two
//! owners in one process and from two processes (this test binary run twice
//! as a child, so nothing per process, such as an address or a hash seed,
//! can reach them). Without a seed they stamp system time and random ids.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use cmux_conversation::{Op, Participant};
use serde_json::{Value, json};

use super::FIXTURE_EPOCH_MS;
use crate::Mux;
use crate::conversation_store::LOCAL_USER;
use crate::switches::Switches;

const SESSION: &str = "oracle-env-stores";
/// Set in the child: the seed whose store bytes it prints.
const CHILD_SEED: &str = "CMUX_ORACLE_ENV_STORES_CHILD_SEED";
const CHILD_TEST: &str = "oracle_env::stores_tests::converted_stores_of_the_child_seed";
const MARK: &str = "converted-stores:";

pub(super) struct StateRoot(pub(super) PathBuf);

impl StateRoot {
    pub(super) fn new() -> Self {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).expect("random");
        let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Self(std::env::temp_dir().join(format!("cmux-oracle-env-{name}")))
    }
}

impl Drop for StateRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn mutate(mux: &Arc<Mux>, operation: &str, mut params: Value, key: &str) -> Value {
    params["machine"] = json!("current");
    params["session"] = json!("current");
    let envelope = json!({
        "protocol": "cmux.protocol/2",
        "type": "request",
        "id": key,
        "operation": operation,
        "params": params,
        "idempotency_key": key,
    });
    let reply = crate::resource_router::handle_resource_message(mux, &envelope.to_string())
        .unwrap_or_else(|error| panic!("{operation}: {error:?}"));
    assert_eq!(reply["ok"], true, "{operation}: {reply}");
    reply["result"]["value"].clone()
}

/// What the converted stores hold after one fixed script, in their own
/// wire shapes. The workspace id is the registry's (not converted yet), so
/// the checklist shows only its items.
fn converted_stores(mux: &Arc<Mux>) -> Value {
    let people: Vec<Participant> = serde_json::from_value(json!([
        {"id": LOCAL_USER, "kind": "human", "display_name": "Me"},
        {"id": "agent_chief", "kind": "agent", "display_name": "Chief", "agent_class": "mux"},
    ]))
    .unwrap();
    let send: Op = serde_json::from_value(json!({
        "kind": "message.send",
        "client_msg_id": "send-one",
        "parts": [{"type": "text", "text": "hello"}],
    }))
    .unwrap();
    let clock = mux.oracle_env().virtual_clock();
    let conversations = mux
        .with_conversations(|store| {
            // Two creates in the same millisecond: the list's order is the id tie-break.
            let one = store.create("create-one", LOCAL_USER, "one", &people)?.summary.id;
            store.create("create-two", LOCAL_USER, "two", &people)?;
            if let Some(clock) = clock {
                clock.advance(1_000)?;
            }
            store.apply_op(&one, "send-one", LOCAL_USER, &send)?;
            Ok(json!({"list": store.list()?, "one": store.snapshot(&one, 10)?}))
        })
        .unwrap();
    let workspace = mutate(
        mux,
        "workspace.create",
        json!({"name": "checklist", "initial_content": "empty"}),
        "create-workspace",
    )["workspace_id"]
        .clone();
    mutate(mux, "workspace_checklist.add", json!({"workspace": workspace, "text": "a"}), "add-a");
    let added = mutate(
        mux,
        "workspace_checklist.add",
        json!({"workspace": workspace, "text": "b", "index": 0}),
        "add-b",
    );
    let item = added["items"][1]["id"].clone();
    let updated = mutate(
        mux,
        "workspace_checklist.update",
        json!({"workspace": workspace, "item": item, "state": "done",
               "attachments": [{"path": "/tmp/shot.png"}]}),
        "update-a",
    );
    json!({
        "conversations": conversations,
        "checklist": updated["items"],
        "checkpoints": crate::git_ops::checkpoint_oracle_sample(mux),
    })
}

fn seeded_bytes(seed: &str) -> String {
    let root = StateRoot::new();
    let mux = Mux::new_for_test_with_switches(SESSION, Some(&root.0), Switches::seeded(seed));
    serde_json::to_string(&converted_stores(&mux)).unwrap()
}

/// The child half of the two-process test; a plain run has no seed and
/// does nothing.
#[test]
fn converted_stores_of_the_child_seed() {
    let Ok(seed) = std::env::var(CHILD_SEED) else { return };
    println!("{MARK}{}", seeded_bytes(&seed));
}

fn child_bytes(seed: &str) -> String {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD_SEED, seed)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    stdout
        .lines()
        .find_map(|line| line.split_once(MARK).map(|(_, bytes)| bytes))
        .unwrap_or_else(|| panic!("the child printed no store bytes: {stdout}"))
        .to_string()
}

#[test]
fn one_seed_gives_byte_equal_converted_stores_across_two_processes() {
    let first = child_bytes("alpha/case-001");
    assert_eq!(first, child_bytes("alpha/case-001"));
    assert_eq!(first, seeded_bytes("alpha/case-001"), "and in this process");
    assert_ne!(first, child_bytes("alpha/case-002"), "another seed, other ids");
}

#[test]
fn seeded_stores_stamp_virtual_time() {
    let root = StateRoot::new();
    let mux = Mux::new_for_test_with_switches(SESSION, Some(&root.0), Switches::seeded("time"));
    let stores = converted_stores(&mux);
    let list = stores["conversations"]["list"].as_array().unwrap();
    let updated: Vec<&str> =
        list.iter().map(|summary| summary["updated_at"].as_str().unwrap()).collect();
    // `one` got its message a virtual second after both creates.
    assert_eq!(updated, ["2026-01-01T00:00:01.000Z", "2026-01-01T00:00:00.000Z"], "{list:?}");
    let checklist = stores["checklist"].as_array().unwrap();
    assert_eq!(checklist[0]["created_at_ms"], (FIXTURE_EPOCH_MS + 1_000).to_string());
    assert_eq!(stores["checkpoints"]["now_ms"], FIXTURE_EPOCH_MS + 1_000);
}

/// Creates in one virtual millisecond tie on `updated_at`; the list orders
/// them by id, newest first, the same in every run.
#[test]
fn conversations_created_in_one_millisecond_list_by_id() {
    let env = super::OracleEnv::seeded("ties");
    let mut store = crate::conversation_store::ConversationStore::open(None, env).unwrap();
    let people: Vec<Participant> =
        serde_json::from_value(json!([{"id": LOCAL_USER, "kind": "human", "display_name": "Me"}]))
            .unwrap();
    let mut created: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|key| store.create(key, LOCAL_USER, key, &people).unwrap().summary.id)
        .collect();
    let listed: Vec<String> = store.list().unwrap().into_iter().map(|summary| summary.id).collect();
    created.sort_unstable_by(|left, right| right.cmp(left));
    assert_eq!(listed, created);
}

/// No seed is the production path: the system clock and fresh random ids,
/// so two owners never mint the same id.
#[test]
fn without_a_seed_the_stores_stamp_system_time_and_random_ids() {
    let run = |root: &Path| {
        let mux = Mux::new_for_test_with_switches(SESSION, Some(root), Switches::default());
        assert!(mux.oracle_env().virtual_clock().is_none());
        converted_stores(&mux)
    };
    let (one, two) = (StateRoot::new(), StateRoot::new());
    let (first, second) = (run(&one.0), run(&two.0));
    for path in ["/checkpoints/repository", "/checkpoints/checkpoint", "/checklist/0/id"] {
        assert_ne!(first.pointer(path), second.pointer(path), "{path}");
    }
    let list_ids = |stores: &Value| stores["conversations"]["list"].to_string();
    assert_ne!(list_ids(&first), list_ids(&second));
    let now = crate::workspace_registry::unix_epoch_ms().unwrap();
    let stamped = first["checkpoints"]["now_ms"].as_u64().unwrap();
    assert!(stamped <= now && now - stamped < 600_000, "{stamped} vs {now}");
}
