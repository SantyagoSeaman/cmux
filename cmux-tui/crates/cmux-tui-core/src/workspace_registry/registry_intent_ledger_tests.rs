//! Workspace and terminal intents are admitted in memory before they are
//! written: the replay check, the revision CAS and the revision the writer
//! writes (p05-ipc-132, `mutation_ledger_cache`).

use super::*;

fn registry_workspace(id: u64, key: &str) -> RegistryWorkspace {
    RegistryWorkspace {
        id,
        public_id: WorkspacePublicId::parse(format!("ws_{id:032x}")).expect("public id"),
        key: key.into(),
        name: "Workspace".into(),
        name_source: Default::default(),
        name_revision: 0,
        group_key: "default".into(),
    }
}

fn create_fingerprint(key: &str) -> Value {
    serde_json::json!({"op": "create", "key": key})
}

fn create_intent(
    registry: &WorkspaceRegistry,
    mutation_id: &str,
    key: &str,
    expected_revision: Option<u64>,
) -> WorkspaceCommitIntent {
    let mutation = WorkspaceMutation::daemon(mutation_id, "test").expect("mutation");
    registry
        .prepare_workspace_commit(
            &mutation,
            &create_fingerprint(key),
            None,
            expected_revision,
            "workspace-added",
            key,
            &[registry_workspace(1, key)],
            None,
            &serde_json::json!({"key": key}),
            false,
        )
        .expect("prepare")
}

fn ledger_rows(registry: &WorkspaceRegistry, mutation_id: &str) -> Vec<u64> {
    let db = registry.connection.get();
    let mut statement = db
        .prepare("SELECT committed_revision FROM mutations WHERE mutation_id = ?1")
        .expect("statement");
    statement
        .query_map([mutation_id], |row| row.get::<_, i64>(0))
        .expect("query")
        .map(|revision| u64::try_from(revision.expect("row")).expect("revision"))
        .collect()
}

fn stored_workspace_revision(registry: &WorkspaceRegistry) -> u64 {
    current_revision(&registry.connection.get()).expect("revision")
}

fn into_commit(receipt: RegistryReceipt) -> RegistryCommit {
    receipt.into_workspace().expect("workspace receipt").commit
}

#[test]
fn the_same_mutation_admitted_back_to_back_applies_once_and_replays() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-back-to-back").expect("registry");
    let base = stored_workspace_revision(&registry);
    let mut first = create_intent(&registry, "create-one", "one", Some(base));
    assert!(registry.admit_ledger_intent(&mut first).expect("admit").is_none());
    let admitted = first.admitted_revision().expect("admitted");
    assert_eq!(admitted, base + 1);

    // The retry arrives before the first commit answers: replayed, not applied.
    let mut retry = create_intent(&registry, "create-one", "one", Some(base));
    let replayed = registry.admit_ledger_intent(&mut retry).expect("admit").expect("replayed");
    let replayed = into_commit(replayed);
    assert!(replayed.replayed);
    assert_eq!(replayed.revision, admitted);
    assert_eq!(retry.admitted_revision(), None);

    let receipt =
        registry.commit_registry_intent_locally(&RegistryIntent::Workspace(first)).expect("commit");
    let committed = into_commit(receipt);
    assert!(!committed.replayed);
    assert_eq!(committed.revision, admitted, "the writer writes the admitted revision");
    assert_eq!(ledger_rows(&registry, "create-one"), [admitted]);
}

#[test]
fn a_commit_replies_the_revision_its_ledger_row_committed() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-reply-revision").expect("registry");
    let base = stored_workspace_revision(&registry);
    let intent = create_intent(&registry, "create-reply", "reply", Some(base));
    let committed =
        into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(intent)).unwrap());
    assert_eq!(ledger_rows(&registry, "create-reply"), [committed.revision]);
    assert_eq!(stored_workspace_revision(&registry), committed.revision);
    let head = registry.ledger_cache.head(mutation_ledger::KeyedLedger::Workspace);
    assert_eq!(head, committed.revision);

    // Committed: a later retry replays from the cache at the same revision.
    let retry = create_intent(&registry, "create-reply", "reply", Some(base));
    let replayed =
        into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(retry)).unwrap());
    assert!(replayed.replayed);
    assert_eq!(replayed.revision, committed.revision);
    assert_eq!(ledger_rows(&registry, "create-reply").len(), 1);
}

#[test]
fn a_revision_conflict_is_refused_at_admission_before_any_write() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-cas").expect("registry");
    let base = stored_workspace_revision(&registry);
    let mut first = create_intent(&registry, "create-first", "first", Some(base));
    assert!(registry.admit_ledger_intent(&mut first).expect("admit").is_none());
    let mut stale = create_intent(&registry, "create-stale", "stale", Some(base));
    let conflict = registry.admit_ledger_intent(&mut stale).expect_err("stale base");
    assert_eq!(
        conflict.to_string(),
        format!("workspace revision conflict: expected {base}, current {}", base + 1)
    );
    assert_eq!(stale.admitted_revision(), None);
    assert_eq!(stored_workspace_revision(&registry), base, "nothing was written");
    assert!(ledger_rows(&registry, "create-stale").is_empty());
}

#[test]
fn a_conflicting_retry_the_cache_never_saw_replays_from_sqlite() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-uncached-retry").expect("registry");
    let base = stored_workspace_revision(&registry);
    // A store that commits its own transaction, as older than the cache.
    let mutation = WorkspaceMutation::daemon("create-direct", "test").expect("mutation");
    let fingerprint = canonical_json(&create_fingerprint("direct")).expect("fingerprint");
    let result_json = canonical_json(&serde_json::json!({"key": "direct"})).expect("result");
    let (revision, _) = {
        let db = registry.connection.get();
        let tx = db.unchecked_transaction().expect("transaction");
        let written = commit_workspace_registry_in_transaction(
            &tx,
            &mutation,
            &fingerprint,
            Some(base),
            "workspace-added",
            "direct",
            &[registry_workspace(1, "direct")],
            &result_json,
        )
        .expect("direct commit");
        tx.commit().expect("commit");
        written
    };
    assert!(registry.ledger_cache.is_stale());

    let mut retry = create_intent(&registry, "create-direct", "direct", Some(base));
    let replayed = registry.admit_ledger_intent(&mut retry).expect("replay").expect("replayed");
    let replayed = into_commit(replayed);
    assert!(replayed.replayed);
    assert_eq!(replayed.revision, revision);
    assert_eq!(ledger_rows(&registry, "create-direct"), [revision]);
}

#[test]
fn a_writer_retry_rewrites_the_same_admitted_revision() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-writer-retry").expect("registry");
    let base = stored_workspace_revision(&registry);
    let mut intent = create_intent(&registry, "create-retry", "retry", Some(base));
    assert!(registry.admit_ledger_intent(&mut intent).expect("admit").is_none());
    let admitted = intent.admitted_revision().expect("admitted");
    let intent = RegistryIntent::Workspace(intent);
    {
        // The batch fails after the intent applied: the writer rolls it back.
        let db = registry.connection.get();
        let tx = db.unchecked_transaction().expect("transaction");
        let receipt = intent.apply_in_savepoint(&tx).expect("batch").expect("intent");
        assert_eq!(into_commit(receipt).revision, admitted);
    }
    let db = registry.connection.get();
    let tx = db.unchecked_transaction().expect("transaction");
    let receipt = intent.apply_in_savepoint(&tx).expect("batch").expect("intent");
    tx.commit().expect("commit");
    drop(db);
    assert_eq!(into_commit(receipt).revision, admitted);
    assert_eq!(stored_workspace_revision(&registry), admitted);
    assert!(!registry.ledger_cache.is_stale(), "an admitted apply is not a foreign write");
}

#[test]
fn a_store_that_commits_directly_is_seen_at_the_next_admission() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-direct-store").expect("registry");
    let base = stored_workspace_revision(&registry);
    registry
        .connection
        .get()
        .execute("UPDATE meta SET value = ?1 WHERE key = 'revision'", [(base + 5).to_string()])
        .expect("direct write");
    assert!(registry.ledger_cache.is_stale());
    let mut intent = create_intent(&registry, "create-after", "after", Some(base + 5));
    assert!(registry.admit_ledger_intent(&mut intent).expect("admit").is_none());
    assert_eq!(intent.admitted_revision(), Some(base + 6));
}

#[test]
fn terminals_committed_together_keep_contiguous_revisions_when_one_fails() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-terminals").expect("registry");
    let base = stored_workspace_revision(&registry);
    let workspace = create_intent(&registry, "create-home", "home", Some(base));
    registry.commit_registry_intent(RegistryIntent::Workspace(workspace)).expect("workspace");
    let terminal_base = registry.terminal_revision().expect("terminal revision");
    let reserve = |registry: &WorkspaceRegistry, id: &str, workspace_key: &str| {
        let terminal = RegistryTerminal {
            terminal_id: id.into(),
            workspace_key: workspace_key.into(),
            incarnation: None,
            lifecycle: TerminalLifecycle::Launching,
            launch_spec: serde_json::json!({
                "command": ["/bin/zsh"], "cwd": "/tmp", "rows": 24, "cols": 80
            }),
            exit: None,
            on_exit: TerminalOnExit::Close,
        };
        let mutation = WorkspaceMutation::daemon(format!("reserve-{id}"), "test").unwrap();
        registry
            .prepare_terminal_commit(
                &mutation,
                &serde_json::json!({"op": "reserve", "id": id}),
                None,
                None,
                "terminal-reserved",
                &terminal,
                &serde_json::json!({"terminal_id": id}),
            )
            .expect("prepare")
    };
    let refused = reserve(&registry, "00000000000040008000000000000001", "missing");
    let kept = reserve(&registry, "00000000000040008000000000000002", "home");
    let results = registry.commit_terminals_together(vec![refused, kept]).expect("commit");
    assert!(results[0].is_err(), "no live workspace: {results:?}");
    let kept = results[1].as_ref().expect("reserved");
    assert_eq!(kept.revision, terminal_base + 1, "the refused one gave its revision back");
    assert_eq!(registry.terminal_revision().expect("terminal revision"), terminal_base + 1);
    let head = registry.ledger_cache.head(mutation_ledger::KeyedLedger::Terminal);
    assert_eq!(head, terminal_base + 1);
}
