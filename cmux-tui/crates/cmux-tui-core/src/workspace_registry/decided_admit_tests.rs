//! The direct registry commits reply at admit (F10, p05-ipc-134): with
//! `admitThenReply` on, a workspace rename and `commit_terminal` do not wait
//! for the journal writer, and the writer then commits exactly the revision
//! the reply gave. Tests of the rename are named `workspace_rename_*`.

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use super::*;
use crate::request_path::{BlockingKind, RequestPathMode, RequestPathScope, RequestPathStats};
use crate::{Mux, SurfaceOptions, WorkspaceMutationResult};

/// The journal writer delay the acceptance names.
const WRITER_DELAY: Duration = Duration::from_millis(50);

fn renaming_mux(name: &str, admit: bool) -> (Arc<Mux>, PathBuf, String) {
    let root = std::env::temp_dir().join(format!("cmux-workspace-rename-{name}-{}", new_uuid_v4()));
    let mux = Mux::open_persistent(name, SurfaceOptions::default(), &root).expect("mux");
    mux.registry_connection.admit.set_enabled_for_test(admit);
    let key = mux.create_empty_workspace(Some("before".into()), None, None).expect("created").key;
    settle_writer(&mux);
    (mux, root, key)
}

/// A cold read: it returns once the writer answered every admitted intent.
fn settle_writer(mux: &Mux) -> RegistrySnapshot {
    mux.workspace_registry.lock().unwrap().snapshot().expect("snapshot")
}

fn rename(
    mux: &Mux,
    key: &str,
    name: &str,
    mutation: &str,
) -> anyhow::Result<WorkspaceMutationResult> {
    mux.rename_workspace_with_mutation(
        None,
        Some(key),
        name.into(),
        None,
        None,
        &WorkspaceMutation::daemon(mutation, "test").unwrap(),
    )
}

fn stored_name(snapshot: &RegistrySnapshot, key: &str) -> Option<String> {
    snapshot.workspaces.iter().find(|workspace| workspace.key == key).map(|w| w.name.clone())
}

/// Red on the base: the rename sent its intent to the writer and waited for
/// the receipt, so it could not reply while the writer was held (a
/// journal_wait at the rename). Here it replies with the prepared revision,
/// and the writer commits that revision once released.
#[test]
fn workspace_rename_replies_while_the_journal_writer_is_held() {
    let (mux, root, key) = renaming_mux("held", true);
    let base = settle_writer(&mux);
    let admits = mux.registry_connection.admit_stats();
    let (entered, writer_entered) = sync_channel(1);
    let (release_writer, release) = sync_channel(1);
    mux.install_journal_before_commit_for_test(entered, release);

    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let (replied, reply) = sync_channel(1);
    let request = std::thread::spawn({
        let (mux, stats, key) = (mux.clone(), stats.clone(), key.clone());
        move || {
            let _scope = RequestPathScope::enter(&stats);
            let renamed = rename(&mux, &key, "after", "rename-held");
            replied.send(renamed.map_err(|error| format!("{error:#}"))).unwrap();
        }
    });

    let renamed = reply
        .recv_timeout(Duration::from_secs(5))
        .expect("the rename must not wait for the held journal writer")
        .expect("renamed");
    assert_eq!(
        (renamed.revision, renamed.replayed, renamed.changed),
        (base.revision + 1, false, true)
    );
    assert_eq!(stats.count(BlockingKind::JournalWait), 0, "{:?}", stats.sites());
    let admitted = mux.registry_connection.admit_stats();
    assert_eq!(admitted.admitted, admits.admitted + 1, "{admitted:?}");
    assert_eq!(admitted.fallbacks_connection_held, admits.fallbacks_connection_held);
    {
        let state = mux.state.lock().unwrap();
        let workspace = state.workspaces.iter().find(|workspace| workspace.key == key).unwrap();
        assert_eq!(workspace.name, "after", "memory holds the reply before the journal does");
        assert_eq!(state.workspace_revision, renamed.revision);
    }
    request.join().unwrap();
    writer_entered.recv_timeout(Duration::from_secs(10)).expect("the writer took the rename");
    release_writer.send(()).unwrap();

    let snapshot = settle_writer(&mux);
    assert_eq!(snapshot.revision, renamed.revision, "the writer committed the reply");
    assert_eq!(stored_name(&snapshot, &key).as_deref(), Some("after"));
    let settled = mux.registry_connection.admit_stats();
    assert_eq!(settled.journal_degraded, admits.journal_degraded, "{settled:?}");
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

/// Acceptance: with a 50 ms journal writer delay and no earlier admission
/// still unanswered, the rename reply excludes the delay. Each round delays
/// the writer's commit of that round's rename; the next round starts after
/// the writer answered (back to back, see
/// `workspace_rename_back_to_back_waits_for_the_earlier_admission`).
#[test]
fn workspace_rename_replies_exclude_a_50_ms_writer_delay() {
    let (mux, root, key) = renaming_mux("delay", true);
    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let mut replies = Vec::new();
    for round in 0..20 {
        let (entered, writer_entered) = sync_channel(1);
        let (release_writer, release) = sync_channel(1);
        mux.install_journal_before_commit_for_test(entered, release);
        let delayer = std::thread::spawn(move || {
            writer_entered.recv_timeout(Duration::from_secs(10)).expect("the writer took it");
            std::thread::sleep(WRITER_DELAY);
            release_writer.send(()).unwrap();
        });
        let started = Instant::now();
        let renamed = {
            let _scope = RequestPathScope::enter(&stats);
            rename(&mux, &key, &format!("round {round}"), &format!("rename-delay-{round}"))
        }
        .expect("renamed");
        replies.push(started.elapsed());
        assert!(!renamed.replayed);
        delayer.join().unwrap();
        assert_eq!(settle_writer(&mux).revision, renamed.revision);
    }
    replies.sort();
    let p95 = replies[(replies.len() * 95).div_ceil(100) - 1];
    eprintln!(
        "rename reply p50 {:?} p95 {p95:?} with a {WRITER_DELAY:?} writer delay",
        replies[replies.len() / 2]
    );
    assert_eq!(stats.count(BlockingKind::JournalWait), 0, "{:?}", stats.sites());
    assert!(p95 < WRITER_DELAY / 2, "the reply waited for the writer: p95 {p95:?}");
    assert_eq!(mux.registry_connection.admit_stats().journal_degraded, 0);
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

/// The limit of deciding on SQLite: a rename sent while an earlier admitted
/// rename has no writer answer waits for that answer in the read barrier of
/// its first connection acquisition (a counted journal_wait), so its receipt
/// is decided on a SQLite that holds the earlier one. Its reply then
/// includes what is left of the earlier writer delay. Deciding it with no
/// SQLite read needs the in-memory ledger and overlay (p05-ipc-132/133).
#[test]
fn workspace_rename_back_to_back_waits_for_the_earlier_admission() {
    let (mux, root, key) = renaming_mux("back-to-back", true);
    let base = settle_writer(&mux);
    let (entered, writer_entered) = sync_channel(1);
    let (release_writer, release) = sync_channel(1);
    mux.install_journal_before_commit_for_test(entered, release);
    let delayer = std::thread::spawn(move || {
        writer_entered.recv_timeout(Duration::from_secs(10)).expect("the writer took it");
        std::thread::sleep(WRITER_DELAY);
        release_writer.send(()).unwrap();
    });
    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let (first, second, second_reply) = {
        let _scope = RequestPathScope::enter(&stats);
        let first = rename(&mux, &key, "first", "rename-back-1").expect("first");
        let started = Instant::now();
        let second = rename(&mux, &key, "second", "rename-back-2").expect("second");
        (first, second, started.elapsed())
    };
    delayer.join().unwrap();
    eprintln!("back-to-back rename reply {second_reply:?} with a {WRITER_DELAY:?} writer delay");
    assert_eq!(first.revision, base.revision + 1);
    assert_eq!(second.revision, first.revision + 1, "decided after the first, never a duplicate");
    assert!(stats.count(BlockingKind::JournalWait) >= 1, "{:?}", stats.sites());
    let snapshot = settle_writer(&mux);
    assert_eq!(snapshot.revision, second.revision);
    assert_eq!(stored_name(&snapshot, &key).as_deref(), Some("second"));
    assert_eq!(mux.registry_connection.admit_stats().journal_degraded, 0);
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

/// Red before the decide checked the barrier: with the writer stalled past
/// the read barrier's bound on an admitted rename, the next rename was
/// decided on a SQLite without it (it failed there with "the resource
/// revision changed"). Now it is refused at the request with the retryable
/// `AdmittedAnswerMissing`: nothing is admitted or degraded, and a retry
/// after the writer answers is decided after the first.
#[test]
fn workspace_rename_after_a_stalled_admission_is_refused_not_decided_stale() {
    let (mux, root, key) = renaming_mux("stalled", true);
    let base = settle_writer(&mux);
    let (entered, writer_entered) = sync_channel(1);
    let (release_writer, release) = sync_channel(1);
    mux.install_journal_before_lock_for_test(entered, release);
    let first = rename(&mux, &key, "first", "rename-stalled-1").expect("first");
    writer_entered.recv_timeout(Duration::from_secs(10)).expect("the writer took the rename");
    let admits = mux.registry_connection.admit_stats();

    let refused = rename(&mux, &key, "second", "rename-stalled-2");
    let stats = mux.registry_connection.admit_stats();
    let refused = refused.expect_err("a rename decided behind an unanswered admission");
    assert!(refused.downcast_ref::<AdmittedAnswerMissing>().is_some(), "{refused:#}");
    assert_eq!(stats.admitted, admits.admitted, "{stats:?}");
    assert_eq!(stats.refusals_unanswered, admits.refusals_unanswered + 1, "{stats:?}");
    {
        let state = mux.state.lock().unwrap();
        let workspace = state.workspaces.iter().find(|workspace| workspace.key == key).unwrap();
        assert_eq!(workspace.name, "first", "a refused rename changes nothing");
        assert_eq!(state.workspace_revision, first.revision);
    }
    release_writer.send(()).unwrap();

    let snapshot = settle_writer(&mux);
    assert_eq!(snapshot.revision, base.revision + 1);
    assert_eq!(stored_name(&snapshot, &key).as_deref(), Some("first"));
    let retried = rename(&mux, &key, "second", "rename-stalled-2").expect("retried");
    assert_eq!((retried.revision, retried.replayed), (first.revision + 1, false));
    let snapshot = settle_writer(&mux);
    assert_eq!(snapshot.revision, retried.revision);
    assert_eq!(stored_name(&snapshot, &key).as_deref(), Some("second"));
    assert_eq!(mux.registry_connection.admit_stats().journal_degraded, 0);
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

/// With the feature off the rename commits through the writer and waits,
/// as before, and the fallback is counted.
#[test]
fn workspace_rename_with_admit_off_waits_for_the_writer_receipt() {
    let (mux, root, key) = renaming_mux("off", false);
    let admits = mux.registry_connection.admit_stats();
    let base = settle_writer(&mux);
    let renamed = rename(&mux, &key, "after", "rename-off").expect("renamed");
    assert_eq!(renamed.revision, base.revision + 1);
    let stats = mux.registry_connection.admit_stats();
    assert_eq!(stats.admitted, admits.admitted, "{stats:?}");
    assert_eq!(stats.fallbacks_disabled, admits.fallbacks_disabled + 1, "{stats:?}");
    let snapshot = settle_writer(&mux);
    assert_eq!(snapshot.revision, renamed.revision);
    assert_eq!(stored_name(&snapshot, &key).as_deref(), Some("after"));
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

/// A repeated rename mutation replays its first answer and admits nothing.
#[test]
fn workspace_rename_replayed_admits_nothing() {
    let (mux, root, key) = renaming_mux("replay", true);
    let first = rename(&mux, &key, "after", "rename-replay").expect("renamed");
    let admits = mux.registry_connection.admit_stats();
    let again = rename(&mux, &key, "after", "rename-replay").expect("replayed");
    assert_eq!((again.revision, again.replayed), (first.revision, true));
    assert_eq!(mux.registry_connection.admit_stats().admitted, admits.admitted);
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

const TERMINAL_ID: &str = "00000000000040008000000000001341";

fn terminal_row(workspace_key: &str, lifecycle: TerminalLifecycle) -> RegistryTerminal {
    RegistryTerminal {
        terminal_id: TERMINAL_ID.into(),
        workspace_key: workspace_key.into(),
        incarnation: None,
        lifecycle,
        launch_spec: serde_json::json!({"command":["/bin/sh"],"cwd":"/tmp","rows":24,"cols":80}),
        exit: (lifecycle == TerminalLifecycle::Exited).then(|| {
            serde_json::json!({
                "outcome": {"kind": "signal", "signal": 15, "core_dumped": false},
                "exited_at": "1340000",
                "revision": "1",
            })
        }),
        on_exit: TerminalOnExit::Close,
    }
}

fn commit_row(
    registry: &mut WorkspaceRegistry,
    mutation: &str,
    expected_revision: Option<u64>,
    terminal: &RegistryTerminal,
) -> anyhow::Result<TerminalRegistryCommit> {
    registry.commit_terminal(
        &WorkspaceMutation::daemon(mutation, "test").unwrap(),
        &serde_json::json!({"op": mutation, "terminal_id": TERMINAL_ID}),
        None,
        expected_revision,
        "terminal-test",
        terminal,
        &serde_json::json!({"terminal_id": TERMINAL_ID}),
    )
}

/// Red on the base: `commit_terminal` sent its intent to the writer and
/// waited for the receipt. Here the reservation replies while the writer is
/// held, and the writer then commits the revision the reply gave.
#[test]
fn commit_terminal_replies_while_the_journal_writer_is_held() {
    let (mux, root, key) = renaming_mux("terminal", true);
    let admits = mux.registry_connection.admit_stats();
    let base = mux.workspace_registry.lock().unwrap().terminal_snapshot().unwrap().revision;
    let (entered, writer_entered) = sync_channel(1);
    let (release_writer, release) = sync_channel(1);
    mux.install_journal_before_commit_for_test(entered, release);

    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let (replied, reply) = sync_channel(1);
    let request = std::thread::spawn({
        let (mux, stats) = (mux.clone(), stats.clone());
        move || {
            let _scope = RequestPathScope::enter(&stats);
            let mut registry = mux.workspace_registry.lock().unwrap();
            let launching = terminal_row(&key, TerminalLifecycle::Launching);
            let reserved = commit_row(&mut registry, "reserve", Some(base), &launching);
            replied.send(reserved.map_err(|error| format!("{error:#}"))).unwrap();
        }
    });

    let reserved = reply
        .recv_timeout(Duration::from_secs(5))
        .expect("commit_terminal must not wait for the held journal writer")
        .expect("reserved");
    assert_eq!((reserved.revision, reserved.replayed), (base + 1, false));
    assert_eq!(stats.count(BlockingKind::JournalWait), 0, "{:?}", stats.sites());
    let admitted = mux.registry_connection.admit_stats();
    assert_eq!(admitted.admitted, admits.admitted + 1, "{admitted:?}");
    request.join().unwrap();
    writer_entered.recv_timeout(Duration::from_secs(10)).expect("the writer took the row");
    release_writer.send(()).unwrap();

    let registry = mux.workspace_registry.lock().unwrap();
    assert_eq!(registry.terminal_snapshot().unwrap().revision, reserved.revision);
    let stored = registry.terminal_record(TERMINAL_ID).unwrap().expect("the row");
    assert_eq!(stored.lifecycle, TerminalLifecycle::Launching);
    drop(registry);
    let settled = mux.registry_connection.admit_stats();
    assert_eq!(settled.journal_degraded, admits.journal_degraded, "{settled:?}");
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

/// What the writer would refuse or latch is decided at the request: a
/// revision conflict is an error and a second exit keeps the first; neither
/// admits anything, so the journal never goes degraded.
#[test]
fn commit_terminal_refusals_and_latches_admit_nothing() {
    let (mux, root, key) = renaming_mux("terminal-refused", true);
    let mut registry = mux.workspace_registry.lock().unwrap();
    let base = registry.terminal_snapshot().unwrap().revision;
    let launching = terminal_row(&key, TerminalLifecycle::Launching);
    let admits = mux.registry_connection.admit_stats();
    commit_row(&mut registry, "reserve", Some(base), &launching).expect("reserved");
    let exited =
        commit_row(&mut registry, "exit", None, &terminal_row(&key, TerminalLifecycle::Exited))
            .expect("exited");
    assert_eq!(exited.revision, base + 2, "the exit is decided after the reservation");
    let stored = registry.terminal_record(TERMINAL_ID).unwrap().expect("the row");
    assert_eq!(stored.lifecycle, TerminalLifecycle::Exited, "the later row landed last");
    let after = mux.registry_connection.admit_stats();
    assert_eq!(after.admitted, admits.admitted + 2, "{after:?}");
    let admits = after;

    let error = commit_row(&mut registry, "stale", Some(base), &launching)
        .expect_err("a stale revision is refused");
    assert!(format!("{error:#}").contains("terminal revision conflict"), "{error:#}");
    let latched = commit_row(
        &mut registry,
        "exit-again",
        None,
        &terminal_row(&key, TerminalLifecycle::Exited),
    )
    .expect("the exit latch");
    assert_eq!((latched.revision, latched.replayed), (base + 2, true));

    let stats = mux.registry_connection.admit_stats();
    assert_eq!(stats.admitted, admits.admitted, "{stats:?}");
    assert_eq!(registry.terminal_snapshot().unwrap().revision, base + 2);
    assert_eq!(mux.registry_connection.admit_stats().journal_degraded, 0);
    drop(registry);
    drop(mux);
    let _ = fs::remove_dir_all(root);
}

/// As for the rename: a terminal commit sent while an admitted one is
/// stalled past the read barrier's bound is refused (retry). Red before: a
/// second reservation was decided on the terminal revision SQLite held
/// before the admitted one and replied that same revision.
#[test]
fn commit_terminal_after_a_stalled_admission_is_refused_not_decided_stale() {
    let (mux, root, key) = renaming_mux("terminal-stalled", true);
    let mut registry = mux.workspace_registry.lock().unwrap();
    let base = registry.terminal_snapshot().unwrap().revision;
    let (entered, writer_entered) = sync_channel(1);
    let (release_writer, release) = sync_channel(1);
    mux.install_journal_before_lock_for_test(entered, release);
    let launching = terminal_row(&key, TerminalLifecycle::Launching);
    let reserved = commit_row(&mut registry, "reserve", Some(base), &launching).expect("reserved");
    assert_eq!(reserved.revision, base + 1);
    writer_entered.recv_timeout(Duration::from_secs(10)).expect("the writer took the row");
    let admits = mux.registry_connection.admit_stats();

    let other = RegistryTerminal {
        terminal_id: "00000000000040008000000000001342".into(),
        ..terminal_row(&key, TerminalLifecycle::Launching)
    };
    let refused = commit_row(&mut registry, "reserve-other", None, &other);
    let stats = mux.registry_connection.admit_stats();
    let refused = refused.expect_err("a reservation decided behind an unanswered admission");
    assert!(refused.downcast_ref::<AdmittedAnswerMissing>().is_some(), "{refused:#}");
    assert_eq!(stats.admitted, admits.admitted, "{stats:?}");
    assert_eq!(stats.refusals_unanswered, admits.refusals_unanswered + 1, "{stats:?}");
    release_writer.send(()).unwrap();

    let retried = commit_row(&mut registry, "reserve-other", None, &other).expect("retried");
    assert_eq!((retried.revision, retried.replayed), (base + 2, false));
    assert_eq!(registry.terminal_snapshot().unwrap().revision, base + 2);
    assert!(registry.terminal_record(TERMINAL_ID).unwrap().is_some(), "the first row");
    assert!(registry.terminal_record(&other.terminal_id).unwrap().is_some(), "the second row");
    drop(registry);
    assert_eq!(mux.registry_connection.admit_stats().journal_degraded, 0);
    drop(mux);
    let _ = fs::remove_dir_all(root);
}
