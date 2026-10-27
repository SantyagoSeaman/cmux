//! Workspace registry commits are admitted, not waited for (F10,
//! p05-ipc-124): the request replies with the prepared revision while the
//! journal writer has not committed it, and the rows the commit also writes
//! ride the admitted intent instead of a request-side transaction.

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use super::*;
use crate::request_path::{BlockingKind, RequestPathMode, RequestPathScope, RequestPathStats};
use crate::{Mux, SurfaceOptions};

/// The `meta` row the staged commit's extra write leaves.
const EXTRA_KEY: &str = "workspace-admit-test-extra";

fn stage_mutation() -> WorkspaceMutation {
    WorkspaceMutation::daemon("workspace-admit-stage", "test").unwrap()
}

fn stage_fingerprint() -> Value {
    serde_json::json!({"op":"create-workspace","key":"staged"})
}

fn admitting_mux(name: &str) -> (Arc<Mux>, PathBuf) {
    let root = std::env::temp_dir().join(format!("cmux-workspace-admit-{name}-{}", new_uuid_v4()));
    let mux = Mux::open_persistent(name, SurfaceOptions::default(), &root).expect("mux");
    mux.registry_connection.admit.set_enabled_for_test(true);
    (mux, root)
}

fn request_side_commits(mux: &Mux) -> u64 {
    mux.registry_connection.write_path_stats().snapshot().request_effect_commits
}

/// `base`'s workspaces plus one more, keyed `key`.
fn with_workspace(base: &RegistrySnapshot, key: &str) -> Vec<RegistryWorkspace> {
    let id = base.workspaces.iter().map(|workspace| workspace.id).max().unwrap_or(0) + 1;
    let mut workspaces = base.workspaces.clone();
    workspaces.push(RegistryWorkspace {
        id,
        public_id: WorkspacePublicId::random().unwrap(),
        key: key.into(),
        name: key.into(),
        name_source: Default::default(),
        name_revision: 0,
        group_key: "default".into(),
    });
    workspaces
}

/// Red on the base: the borrowed extra write forced a request-side
/// transaction, so the request committed the stage itself. Here the writer
/// is held inside its batch until the request has replied.
#[test]
fn a_staged_workspace_row_replies_before_the_writer_commits_it() {
    let (mux, root) = admitting_mux("stage");
    let base = mux.workspace_registry.lock().unwrap().snapshot().unwrap();
    let request_commits = request_side_commits(&mux);
    let (entered, writer_entered) = sync_channel(1);
    let (release_writer, release) = sync_channel(1);
    mux.install_journal_before_commit_for_test(entered, release);

    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let (replied, reply) = sync_channel(1);
    let request = std::thread::spawn({
        let (mux, stats, workspaces) =
            (mux.clone(), stats.clone(), with_workspace(&base, "staged"));
        let expected_revision = Some(base.revision);
        move || {
            let mut registry = mux.workspace_registry.lock().unwrap();
            let _scope = RequestPathScope::enter(&stats);
            let extra: OwnedTransactionWrite = Box::new(|tx| {
                tx.execute("INSERT INTO meta(key, value) VALUES(?1, '1')", [EXTRA_KEY])?;
                Ok(())
            });
            let commit = registry.commit_for_resource_effect_with(
                &stage_mutation(),
                &stage_fingerprint(),
                None,
                expected_revision,
                "workspace-added",
                "staged",
                &workspaces,
                None,
                &serde_json::json!({"key":"staged"}),
                Some(extra),
            );
            let reply = commit
                .map(|commit| (commit.revision, commit.replayed))
                .map_err(|error| format!("{error:#}"));
            replied.send(reply).unwrap();
        }
    });

    let reply = reply
        .recv_timeout(Duration::from_secs(5))
        .expect("the reply must not wait for the held journal writer");
    assert_eq!(reply, Ok((base.revision + 1, false)), "the reply carries the prepared revision");
    assert_eq!(stats.count(BlockingKind::JournalWait), 0, "{:?}", stats.sites());
    request.join().unwrap();
    writer_entered
        .recv_timeout(Duration::from_secs(10))
        .expect("the writer took the admitted stage");
    release_writer.send(()).unwrap();

    let registry = mux.workspace_registry.lock().unwrap();
    let snapshot = registry.snapshot().unwrap();
    assert_eq!(snapshot.revision, base.revision + 1, "read-your-writes after the reply");
    let stored = registry.replay(&stage_mutation(), &stage_fingerprint()).unwrap();
    assert_eq!(
        stored.map(|commit| commit.revision),
        Some(base.revision + 1),
        "the stage's ledger row"
    );
    let extra: Option<String> = registry
        .connection
        .get()
        .query_row("SELECT value FROM meta WHERE key = ?1", [EXTRA_KEY], |row| row.get(0))
        .optional()
        .unwrap();
    assert_eq!(extra.as_deref(), Some("1"), "the extra row rode the admitted intent");
    assert_eq!(request_side_commits(&mux), request_commits, "the writer committed it");
    drop(registry);
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

/// Red on the base: the presentation row was a borrowed write, so every
/// presentation commit ran its own request-side transaction.
#[test]
fn a_presentation_commit_rides_the_admitted_intent() {
    let (mux, root) = admitting_mux("presentation");
    let mut registry = mux.workspace_registry.lock().unwrap();
    let base = registry.snapshot().unwrap();
    let workspaces = with_workspace(&base, "titled");
    let active = workspaces.last().map(|workspace| workspace.public_id.clone());
    let request_commits = request_side_commits(&mux);
    let update =
        WorkspacePresentationUpdate { title: Some(Some("Admitted".into())), ..Default::default() };

    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let commit = {
        let _scope = RequestPathScope::enter(&stats);
        registry
            .commit_workspace_presentation(
                &WorkspaceMutation::daemon("workspace-admit-presentation", "test").unwrap(),
                &serde_json::json!({"op":"set-workspace-metadata","key":"titled"}),
                None,
                Some(base.revision),
                "workspace-added",
                "titled",
                &workspaces,
                active.as_ref(),
                &update,
                &serde_json::json!({"key":"titled"}),
            )
            .expect("admitted")
    };
    assert_eq!((commit.revision, commit.replayed), (base.revision + 1, false));
    assert_eq!(stats.count(BlockingKind::JournalWait), 0, "{:?}", stats.sites());
    assert_eq!(request_side_commits(&mux), request_commits, "no request-side transaction");

    let record = registry.presentation_snapshot().unwrap().workspace("titled").cloned();
    assert_eq!(record.and_then(|record| record.title).as_deref(), Some("Admitted"));
    drop(registry);
    drop(mux);
    let _ = fs::remove_dir_all(root);
}
