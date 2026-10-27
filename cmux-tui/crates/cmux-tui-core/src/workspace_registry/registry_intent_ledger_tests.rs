//! Workspace and terminal intents are admitted in memory before they are
//! written: the replay check, the revision CAS and the revision the writer
//! writes (p05-ipc-132, `mutation_ledger_cache`).

use std::collections::VecDeque;
use std::sync::{Mutex, Weak};
use std::time::Duration;

use cmux_threads::sync::mpsc::{SyncSender, sync_channel};

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
    create_intent_in(registry, mutation_id, key, None, expected_revision)
}

fn create_intent_in(
    registry: &WorkspaceRegistry,
    mutation_id: &str,
    key: &str,
    expected_generation: Option<&str>,
    expected_revision: Option<u64>,
) -> WorkspaceCommitIntent {
    let mutation = WorkspaceMutation::daemon(mutation_id, "test").expect("mutation");
    registry
        .prepare_workspace_commit(
            &mutation,
            &create_fingerprint(key),
            expected_generation,
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

/// What the scripted writer does with the next intent it is sent.
enum WriterStep {
    /// Apply it in a batch transaction and answer.
    Apply,
    /// Answer an error without applying it (the batch was refused).
    Refuse,
    /// Take it but answer late, an indeterminate receipt
    /// ([`ScriptedWriter::answer_held`]).
    Hold,
    /// Hand it back: the writer stopped before it queued.
    NotQueued,
}

type HeldIntent = (RegistryIntent, SyncSender<Result<RegistryReceipt, String>>);

/// The journal writer's part, scripted: it applies an intent in its own
/// batch transaction under the intent's SAVEPOINT on its own thread, as
/// `writer_batch` does, and keeps every intent error it answered.
struct ScriptedWriter {
    connection: Weak<RegistryConnection>,
    steps: Mutex<VecDeque<WriterStep>>,
    held: Mutex<Option<HeldIntent>>,
    applied: Mutex<usize>,
    errors: Mutex<Vec<String>>,
}

impl ScriptedWriter {
    fn install(
        registry: &WorkspaceRegistry,
        steps: impl IntoIterator<Item = WriterStep>,
    ) -> Arc<Self> {
        let writer = Arc::new(Self {
            connection: Arc::downgrade(&registry.connection),
            steps: Mutex::new(steps.into_iter().collect()),
            held: Mutex::new(None),
            applied: Mutex::new(0),
            errors: Mutex::new(Vec::new()),
        });
        let sink: Arc<dyn RegistryIntentSink> = writer.clone();
        registry.connection.install_intent_sink(sink);
        writer
    }

    /// One writer batch with `intent`, on a writer thread.
    fn apply(&self, intent: &RegistryIntent) -> Result<RegistryReceipt, String> {
        let connection = self.connection.upgrade().expect("connection");
        let answer = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let db = connection.get();
                    let tx = db.unchecked_transaction().expect("batch");
                    let answer = intent.apply_in_savepoint(&tx).expect("the batch itself");
                    tx.commit().expect("batch commit");
                    answer
                })
                .join()
                .expect("writer thread")
        });
        *self.applied.lock().unwrap() += 1;
        if let Err(error) = &answer {
            self.errors.lock().unwrap().push(error.clone());
        }
        answer
    }

    /// The late answer of the held intent: the writer applies it now and
    /// answers its request. Returns the committed revision.
    fn answer_held(&self) -> Result<u64, String> {
        let (intent, answer) = self.held.lock().unwrap().take().expect("a held intent");
        let result = self.apply(&intent);
        let revision = result.as_ref().map_err(String::clone).map(|receipt| match receipt {
            RegistryReceipt::Terminal(commit) => commit.revision,
            RegistryReceipt::Workspace(receipt) => receipt.commit.revision,
            RegistryReceipt::Effect(receipt) => receipt.revision(),
        });
        answer.send(result).expect("the request keeps the receipt channel");
        revision
    }

    fn applied(&self) -> usize {
        *self.applied.lock().unwrap()
    }

    fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }
}

impl RegistryIntentSink for ScriptedWriter {
    fn accepts(&self) -> bool {
        true
    }

    fn send(
        &self,
        intent: RegistryIntent,
        _site: &'static std::panic::Location<'static>,
    ) -> EffectSend {
        let step = self.steps.lock().unwrap().pop_front().expect("a scripted writer step");
        match step {
            WriterStep::Apply => {
                EffectSend::Sent(self.apply(&intent).map_err(|error| anyhow::anyhow!(error)))
            }
            WriterStep::Refuse => {
                EffectSend::Sent(Err(anyhow::anyhow!("the writer refused the batch")))
            }
            WriterStep::Hold => {
                let (answer, receipt) = sync_channel(1);
                *self.held.lock().unwrap() = Some((intent, answer));
                EffectSend::Indeterminate(
                    anyhow::anyhow!("the registry commit is indeterminate"),
                    PendingRegistryReceipt::for_test(receipt),
                )
            }
            WriterStep::NotQueued => EffectSend::NotQueued(Box::new(intent)),
        }
    }
}

fn reserve_terminal(
    registry: &WorkspaceRegistry,
    terminal_id: &str,
    workspace_key: &str,
) -> TerminalCommitIntent {
    let terminal = RegistryTerminal {
        terminal_id: terminal_id.into(),
        workspace_key: workspace_key.into(),
        incarnation: None,
        lifecycle: TerminalLifecycle::Launching,
        launch_spec: serde_json::json!({
            "command": ["/bin/zsh"], "cwd": "/tmp", "rows": 24, "cols": 80
        }),
        exit: None,
        on_exit: TerminalOnExit::Close,
    };
    let mutation = WorkspaceMutation::daemon(format!("reserve-{terminal_id}"), "test").unwrap();
    registry
        .prepare_terminal_commit(
            &mutation,
            &serde_json::json!({"op": "reserve", "id": terminal_id}),
            None,
            None,
            "terminal-reserved",
            &terminal,
            &serde_json::json!({"terminal_id": terminal_id}),
        )
        .expect("prepare")
}

#[test]
fn the_writer_applies_a_mutation_once_and_its_retry_replays_the_committed_revision() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-writer-once").expect("registry");
    let writer = ScriptedWriter::install(&registry, [WriterStep::Apply]);
    let base = stored_workspace_revision(&registry);
    let first = create_intent(&registry, "create-once", "once", Some(base));
    let committed =
        into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(first)).unwrap());
    assert!(!committed.replayed);
    assert_eq!(committed.revision, base + 1);

    for expected_revision in [Some(base), None] {
        let retry = create_intent(&registry, "create-once", "once", expected_revision);
        let replayed =
            into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(retry)).unwrap());
        assert!(replayed.replayed);
        assert_eq!(replayed.revision, committed.revision, "the reply is the committed revision");
    }
    assert_eq!(writer.applied(), 1, "the retries replay before the writer");
    assert_eq!(writer.errors(), Vec::<String>::new());
    assert_eq!(ledger_rows(&registry, "create-once"), [committed.revision]);
    assert_eq!(stored_workspace_revision(&registry), committed.revision);
}

#[test]
fn an_indeterminate_intent_commits_at_its_admitted_revision_and_its_retry_replays() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-writer-late").expect("registry");
    let writer = ScriptedWriter::install(&registry, [WriterStep::Hold]);
    let base = stored_workspace_revision(&registry);
    let late = create_intent(&registry, "create-late", "late", Some(base));
    let error = registry.commit_registry_intent(RegistryIntent::Workspace(late)).unwrap_err();
    assert!(error.to_string().contains("indeterminate"), "{error:#}");
    assert_eq!(registry.ledger_cache.head(mutation_ledger::KeyedLedger::Workspace), base + 1);

    let late = writer.answer_held().expect("the late commit");
    assert_eq!(late, base + 1, "the writer wrote the admitted revision");

    // The retry drains the late answer, then replays it from SQLite.
    let retry = create_intent(&registry, "create-late", "late", Some(base));
    let replayed =
        into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(retry)).unwrap());
    assert!(replayed.replayed);
    assert_eq!(replayed.revision, late);
    assert_eq!(writer.applied(), 1);
    assert_eq!(writer.errors(), Vec::<String>::new());
    assert_eq!(ledger_rows(&registry, "create-late"), [late]);
    let settled = registry.connection.take_settled_receipts();
    assert_eq!(settled.len(), 1, "the late commit still owes its state update");
}

#[test]
fn a_direct_store_waits_for_an_indeterminate_admitted_intent_before_its_revision() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-direct-after-late").expect("registry");
    let writer = ScriptedWriter::install(
        &registry,
        [WriterStep::Apply, WriterStep::Apply, WriterStep::Hold],
    );
    let base = stored_workspace_revision(&registry);
    let home = create_intent(&registry, "create-home", "home", Some(base));
    registry.commit_registry_intent(RegistryIntent::Workspace(home)).expect("workspace");
    let closed_id = "00000000000040008000000000000001";
    let late_id = "00000000000040008000000000000002";
    let closed = reserve_terminal(&registry, closed_id, "home");
    let closed = registry.commit_registry_intent(RegistryIntent::Terminal(closed)).unwrap();
    let terminal_base = closed.into_terminal().expect("terminal").revision;

    let late = reserve_terminal(&registry, late_id, "home");
    let error = registry.commit_registry_intent(RegistryIntent::Terminal(late)).unwrap_err();
    assert!(error.to_string().contains("indeterminate"), "{error:#}");

    // A store that writes the terminal revision in its own transaction runs
    // while the late intent is still queued in the writer.
    let (done, finished) = std::sync::mpsc::channel();
    let (waited, store, late) = std::thread::scope(|scope| {
        let store = scope.spawn(|| {
            let mutation = WorkspaceMutation::daemon("close-direct", "test").unwrap();
            let closed = registry.close_terminal(&mutation, None, None, closed_id, None);
            done.send(()).expect("test thread");
            closed
        });
        let waited = finished.recv_timeout(Duration::from_millis(200)).is_err();
        let late = writer.answer_held();
        (waited, store.join().expect("store thread"), late)
    });
    assert_eq!(writer.errors(), Vec::<String>::new(), "no writer-side revision failure");
    assert_eq!(late, Ok(terminal_base + 1), "the late intent commits at its admitted revision");
    assert!(waited, "the store waited for the late answer");
    let store = store.expect("the direct store commits after it");
    assert_eq!(store.revision, terminal_base + 2);
    assert_eq!(registry.terminal_revision().unwrap(), terminal_base + 2);
}

#[test]
fn a_refused_writer_commit_gives_its_admitted_revision_back() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-writer-refused").expect("registry");
    let writer = ScriptedWriter::install(&registry, [WriterStep::Refuse, WriterStep::Apply]);
    let base = stored_workspace_revision(&registry);
    let refused = create_intent(&registry, "create-refused", "refused", Some(base));
    registry.commit_registry_intent(RegistryIntent::Workspace(refused)).unwrap_err();
    assert_eq!(registry.ledger_cache.head(mutation_ledger::KeyedLedger::Workspace), base);

    let kept = create_intent(&registry, "create-kept", "kept", Some(base));
    let kept =
        into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(kept)).unwrap());
    assert_eq!(kept.revision, base + 1);
    assert!(ledger_rows(&registry, "create-refused").is_empty());
    assert_eq!(writer.applied(), 1);
    assert_eq!(writer.errors(), Vec::<String>::new());
}

#[test]
fn an_intent_the_writer_did_not_queue_commits_locally_at_its_admitted_revision() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-writer-stopped").expect("registry");
    let writer = ScriptedWriter::install(&registry, [WriterStep::NotQueued]);
    let base = stored_workspace_revision(&registry);
    let intent = create_intent(&registry, "create-local", "local", Some(base));
    let committed =
        into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(intent)).unwrap());
    assert!(!committed.replayed);
    assert_eq!(committed.revision, base + 1);
    let cached = registry.ledger_cache.committed(mutation_ledger::KeyedLedger::Workspace);
    assert_eq!(cached, base + 1);
    assert_eq!(writer.applied(), 0);
}

#[test]
fn a_retry_after_a_restart_replays_before_the_generation_check() {
    let root = std::env::temp_dir().join(format!("cmux-registry-ledger-restart-{}", new_uuid_v4()));
    let session = "ledger-restart";
    let mut registry = WorkspaceRegistry::open(&root, session).expect("registry");
    let old_generation = registry.generation().to_string();
    let base = stored_workspace_revision(&registry);
    let first = create_intent_in(&registry, "create-old", "old", Some(&old_generation), Some(base));
    let committed =
        into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(first)).unwrap());
    {
        // Newer rows push the key out of the cache the reopen seeds.
        let db = registry.connection.get();
        let revision = i64::try_from(committed.revision).unwrap();
        for index in 0..mutation_ledger::LEDGER_CACHE_CAPACITY {
            let newer = WorkspaceMutation::daemon(format!("newer-{index}"), "test").unwrap();
            let ledger = mutation_ledger::KeyedLedger::Workspace;
            mutation_ledger::insert_keyed_mutation(&db, ledger, &newer, "{}", "{}", revision)
                .expect("newer row");
        }
    }
    drop(registry);

    let mut registry = WorkspaceRegistry::open(&root, session).expect("reopen");
    assert_ne!(registry.generation(), old_generation, "a new generation per open");
    let retry = create_intent_in(&registry, "create-old", "old", Some(&old_generation), Some(base));
    let replayed =
        into_commit(registry.commit_registry_intent(RegistryIntent::Workspace(retry)).unwrap());
    assert!(replayed.replayed);
    assert_eq!(replayed.revision, committed.revision);

    let fresh = create_intent_in(&registry, "create-new", "new", Some(&old_generation), None);
    let conflict = registry.commit_registry_intent(RegistryIntent::Workspace(fresh)).unwrap_err();
    assert!(conflict.to_string().starts_with("workspace generation conflict"), "{conflict:#}");
    drop(registry);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn an_effect_commit_leaves_the_cached_revisions_alone() {
    let mut registry = WorkspaceRegistry::in_memory("ledger-effect-commit").expect("registry");
    let (key, operation) = ("effect-key", "notification.create");
    let fingerprint = serde_json::json!({"title": "hello"});
    let intent = serde_json::json!({"notification_id": "notification_reserved"});
    registry
        .prepare_resource_effect(key, operation, &fingerprint, &intent, None, Some(0))
        .expect("prepare");
    registry.mark_resource_effect_executing(key, operation, &fingerprint).expect("executing");
    assert!(!registry.ledger_cache.is_stale());
    let outcome = ResourceEffectOutcome::Success(serde_json::json!({"id": "notice"}));
    let deltas = serde_json::json!([{"kind": "upsert"}]);
    let revision = registry
        .commit_resource_effect(key, operation, &fingerprint, &outcome, Some(&deltas))
        .expect("effect commit");
    assert_eq!(revision, 1, "it wrote resource_revision");
    assert_eq!(registry.resource_revision().unwrap(), revision);
    assert!(!registry.ledger_cache.is_stale(), "resource_revision is not a ledger revision");
}
