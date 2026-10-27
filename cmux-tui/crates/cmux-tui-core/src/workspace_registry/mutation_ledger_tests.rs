//! The in-memory revisions and mutation-ledger cache (p05-ipc-132).

use rusqlite::{Connection, params};

use super::*;

fn ledger_database(workspace_revision: u64, terminal_revision: u64) -> Connection {
    let connection = Connection::open_in_memory().expect("database");
    connection
        .execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
             CREATE TABLE mutations(origin TEXT, mutation_id TEXT, fingerprint TEXT,
               result_json TEXT, committed_revision INTEGER, actor TEXT);
             CREATE TABLE terminal_mutations(origin TEXT, mutation_id TEXT, fingerprint TEXT,
               result_json TEXT, committed_revision INTEGER, actor TEXT);",
        )
        .expect("schema");
    connection
        .execute(
            "INSERT INTO meta(key, value) VALUES('revision', ?1), ('terminal_revision', ?2)",
            params![workspace_revision.to_string(), terminal_revision.to_string()],
        )
        .expect("revisions");
    connection
}

fn mutation(id: &str) -> WorkspaceMutation {
    WorkspaceMutation::daemon(id, "test").expect("mutation")
}

fn request<'a>(
    mutation: &'a WorkspaceMutation,
    fingerprint: &'a str,
    expected_revision: Option<u64>,
) -> LedgerRequest<'a> {
    LedgerRequest {
        ledger: KeyedLedger::Workspace,
        mutation,
        fingerprint,
        result_json: "{\"ok\":true}",
        expected_revision,
    }
}

#[test]
fn seeds_the_revisions_and_the_newest_ledger_rows() {
    let connection = ledger_database(3, 7);
    connection
        .execute(
            "INSERT INTO mutations VALUES('test', 'old', '{\"op\":1}', '{\"r\":1}', 3, 'daemon')",
            [],
        )
        .expect("row");
    let cache = LedgerCache::seed(&connection).expect("seed");
    assert_eq!(cache.head(KeyedLedger::Workspace), 3);
    assert_eq!(cache.head(KeyedLedger::Terminal), 7);
    let old = mutation("old");
    let replay = cache.replay(&request(&old, "{\"op\":1}", None)).expect("same payload");
    assert_eq!(replay, Some(LedgerReplay { revision: 3, result_json: "{\"r\":1}".into() }));
    let refused = cache.replay(&request(&old, "{\"op\":2}", None)).expect_err("other payload");
    assert_eq!(refused.to_string(), "mutation old from test was retried with a different payload");
}

#[test]
fn the_same_mutation_admitted_back_to_back_replays_the_admitted_revision() {
    let connection = ledger_database(3, 0);
    let mut cache = LedgerCache::seed(&connection).expect("seed");
    let first = mutation("create");
    assert_eq!(cache.admit(&request(&first, "{}", Some(3))), Ok(Admission::Admitted(4)));
    // Not committed yet: the retry replays the admitted result, no second apply.
    let retry = cache.admit(&request(&first, "{}", Some(3))).expect("replay");
    let Admission::Replay(replay) = retry else { panic!("expected a replay, got {retry:?}") };
    assert_eq!(replay.revision, 4);
    assert_eq!(cache.head(KeyedLedger::Workspace), 4);
}

#[test]
fn the_revision_cas_runs_against_the_newest_admitted_revision() {
    let connection = ledger_database(3, 0);
    let mut cache = LedgerCache::seed(&connection).expect("seed");
    let (first, second) = (mutation("first"), mutation("second"));
    assert_eq!(cache.admit(&request(&first, "{}", Some(3))), Ok(Admission::Admitted(4)));
    let conflict = cache.admit(&request(&second, "{}", Some(3))).expect_err("stale base");
    assert_eq!(
        conflict,
        LedgerRefusal::RevisionConflict { ledger: KeyedLedger::Workspace, expected: 3, current: 4 }
    );
    assert_eq!(conflict.to_string(), "workspace revision conflict: expected 3, current 4");
    assert_eq!(cache.admit(&request(&second, "{}", Some(4))), Ok(Admission::Admitted(5)));
}

#[test]
fn an_unapplied_intent_gives_its_revision_back_and_a_committed_one_keeps_replaying() {
    let connection = ledger_database(0, 0);
    let mut cache = LedgerCache::seed(&connection).expect("seed");
    let (refused, kept) = (mutation("refused"), mutation("kept"));
    let refused_request = request(&refused, "{}", None);
    assert_eq!(cache.admit(&refused_request), Ok(Admission::Admitted(1)));
    cache.settle(&refused_request.ticket(), LedgerOutcome::Unapplied);
    assert_eq!(cache.head(KeyedLedger::Workspace), 0);
    assert_eq!(cache.replay(&refused_request), Ok(None));

    let kept_request = request(&kept, "{}", Some(0));
    assert_eq!(cache.admit(&kept_request), Ok(Admission::Admitted(1)));
    cache.settle(&kept_request.ticket(), LedgerOutcome::Committed(1));
    assert_eq!(cache.committed(KeyedLedger::Workspace), 1);
    let replay = cache.replay(&kept_request).expect("replay").expect("cached");
    assert_eq!(replay.revision, 1);
}

#[test]
fn indeterminate_intents_are_forgotten_once_drained_and_the_revisions_reread() {
    let connection = ledger_database(0, 0);
    let mut cache = LedgerCache::seed(&connection).expect("seed");
    let late = mutation("late");
    let late_request = request(&late, "{}", None);
    assert_eq!(cache.admit(&late_request), Ok(Admission::Admitted(1)));
    cache.settle(&late_request.ticket(), LedgerOutcome::Indeterminate);
    // Still unknown: the next intent cannot reuse its revision.
    assert_eq!(cache.head(KeyedLedger::Workspace), 1);
    cache.reconcile_indeterminate();
    assert!(cache.is_stale());
    // The writer committed it after all.
    connection.execute("UPDATE meta SET value = '1' WHERE key = 'revision'", []).expect("commit");
    cache.refresh(&connection).expect("refresh");
    assert_eq!(cache.head(KeyedLedger::Workspace), 1);
    assert_eq!(cache.replay(&late_request), Ok(None), "a retry replays from the stored ledger");
}

#[test]
fn only_meta_writes_outside_an_admitted_apply_mark_the_cache_stale() {
    let connection = ledger_database(2, 0);
    let mut cache = LedgerCache::seed(&connection).expect("seed");
    {
        let _admitted = AdmittedWriteScope::enter();
        connection
            .execute("UPDATE meta SET value = '3' WHERE key = 'revision'", [])
            .expect("write");
    }
    assert!(!cache.is_stale());
    connection
        .execute("INSERT INTO mutations VALUES('test', 'x', '{}', '{}', 3, 'daemon')", [])
        .expect("ledger row");
    assert!(!cache.is_stale(), "only meta carries the revisions");
    connection.execute("UPDATE meta SET value = '9' WHERE key = 'revision'", []).expect("write");
    assert!(cache.is_stale());
    cache.refresh(&connection).expect("refresh");
    assert!(!cache.is_stale());
    assert_eq!(cache.head(KeyedLedger::Workspace), 9);
}

#[test]
fn a_revision_that_moved_back_drops_the_committed_entries() {
    let connection = ledger_database(5, 0);
    connection
        .execute("INSERT INTO mutations VALUES('test', 'gone', '{}', '{}', 5, 'daemon')", [])
        .expect("row");
    let mut cache = LedgerCache::seed(&connection).expect("seed");
    connection.execute("UPDATE meta SET value = '0' WHERE key = 'revision'", []).expect("reset");
    cache.refresh(&connection).expect("refresh");
    let gone = mutation("gone");
    assert_eq!(cache.replay(&request(&gone, "{}", None)), Ok(None));
    assert_eq!(cache.head(KeyedLedger::Workspace), 0);
}

#[test]
fn the_cache_keeps_a_bounded_number_of_committed_rows() {
    let connection = ledger_database(0, 0);
    let rows = LEDGER_CACHE_CAPACITY + 8;
    for index in 0..rows {
        connection
            .execute(
                "INSERT INTO mutations VALUES('test', ?1, '{}', '{}', ?2, 'daemon')",
                params![format!("m{index}"), i64::try_from(index).expect("index")],
            )
            .expect("row");
    }
    let mut cache = LedgerCache::seed(&connection).expect("seed");
    let (oldest, newest) = (mutation("m0"), mutation(&format!("m{}", rows - 1)));
    assert_eq!(cache.replay(&request(&oldest, "{}", None)), Ok(None));
    assert!(cache.replay(&request(&newest, "{}", None)).expect("replay").is_some());

    let fresh = mutation("fresh");
    let fresh_request = request(&fresh, "{}", None);
    assert_eq!(cache.admit(&fresh_request), Ok(Admission::Admitted(1)));
    cache.settle(&fresh_request.ticket(), LedgerOutcome::Committed(1));
    let evicted = mutation("m8");
    assert_eq!(cache.replay(&request(&evicted, "{}", None)), Ok(None));
    assert_eq!(cache.keyed(KeyedLedger::Workspace).order.len(), LEDGER_CACHE_CAPACITY);
}

#[test]
fn the_writer_writes_the_admitted_revision_and_never_moves_back() {
    let plan = RevisionPlan::Admitted(5);
    assert_eq!(plan.revision(KeyedLedger::Terminal, 3).expect("ahead"), 5);
    assert_eq!(plan.revision(KeyedLedger::Terminal, 3).expect("a retry"), 5);
    let behind = plan.revision(KeyedLedger::Terminal, 5).expect_err("not ahead");
    assert_eq!(behind.to_string(), "admitted terminal revision 5 is not ahead of the committed 5");
    let next = RevisionPlan::Next { expected: Some(2) };
    let conflict = next.revision(KeyedLedger::Terminal, 3).expect_err("stale");
    assert_eq!(conflict.to_string(), "terminal revision conflict: expected 2, current 3");
    assert_eq!(
        RevisionPlan::Next { expected: None }.revision(KeyedLedger::Workspace, 3).ok(),
        Some(4)
    );
}
