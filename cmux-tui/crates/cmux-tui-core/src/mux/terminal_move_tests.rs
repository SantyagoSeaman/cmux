//! A terminal move admits its `terminal-moved` row and replies: no journal
//! writer receipt wait and no request-side commit on the request path (F10,
//! p05-ipc-123).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use super::*;
use crate::request_path::{BlockingKind, RequestPathMode, RequestPathScope, RequestPathStats};

const TERMINAL: &str = "0000000000004000800000000000007b";

struct Fixture {
    mux: Arc<Mux>,
    root: PathBuf,
    from: String,
    to: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A persistent mux (the journal writer runs) with admit then reply on,
/// two workspaces and a terminal row in the first, in `lifecycle`.
fn fixture(name: &str, lifecycle: TerminalLifecycle) -> Fixture {
    let root = std::env::temp_dir()
        .join(format!("cmux-terminal-move-{name}-{}", WorkspacePublicId::random().unwrap()));
    let mux = Mux::open_persistent(name, SurfaceOptions::default(), &root).unwrap();
    mux.registry_connection.set_admit_then_reply_for_test(true);
    let from = mux.create_empty_workspace(Some("from".into()), None, None).unwrap().key;
    let to = mux.create_empty_workspace(Some("to".into()), None, None).unwrap().key;
    let mut terminal = RegistryTerminal {
        terminal_id: TERMINAL.into(),
        workspace_key: from.clone(),
        incarnation: None,
        lifecycle: TerminalLifecycle::Launching,
        launch_spec: serde_json::json!({}),
        exit: None,
        on_exit: TerminalOnExit::Close,
    };
    let mut registry = mux.workspace_registry.lock().unwrap();
    commit_terminal_transition(&mut registry, "terminal-reserved", "reserve-terminal", &terminal)
        .unwrap();
    if lifecycle == TerminalLifecycle::Exited {
        terminal.lifecycle = TerminalLifecycle::Exited;
        terminal.exit = Some(serde_json::json!({
            "outcome":{"kind":"unknown","reason":"test"},
            "exited_at":"17",
            "revision":"1",
        }));
        commit_terminal_transition(&mut registry, "terminal-exited", "exit-terminal", &terminal)
            .unwrap();
    }
    drop(registry);
    Fixture { mux, root, from, to }
}

/// Hold the writer's next batch before it commits.
fn pause_next_writer_batch(mux: &Mux) -> (Receiver<()>, SyncSender<()>) {
    let (entered, writer_entered) = sync_channel(1);
    let (release, release_receiver) = sync_channel(1);
    mux.install_journal_before_commit_for_test(entered, release_receiver);
    (writer_entered, release)
}

fn terminal_revision(mux: &Mux) -> u64 {
    mux.workspace_registry.lock().unwrap().terminal_revision().unwrap()
}

fn move_to(fixture: &Fixture, mutation: &str) -> (anyhow::Result<TerminalMoveResult>, u64) {
    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let _scope = RequestPathScope::enter(&stats);
    let moved = fixture.mux.move_terminal_with_mutation(
        TERMINAL,
        &fixture.to,
        None,
        None,
        None,
        &WorkspaceMutation::daemon(mutation, "test").unwrap(),
    );
    (moved, stats.count(BlockingKind::JournalWait))
}

/// Red on the base: the move committed its row on the request thread while
/// it held the connection pin (a request-side commit and fsync).
#[test]
fn a_terminal_move_replies_before_the_journal_writer_commits_it() {
    let fixture = fixture("admitted", TerminalLifecycle::Launching);
    let mux = &fixture.mux;
    let base = terminal_revision(mux);
    let request_commits = mux.write_path_stats().request_effect_commits;
    let (writer_entered, release) = pause_next_writer_batch(mux);

    let (moved, journal_waits) = move_to(&fixture, "move-admitted");
    let moved = moved.expect("the move replies while the writer holds its batch");
    assert_eq!(journal_waits, 0, "the move must not wait for the journal writer");
    assert_eq!(
        mux.write_path_stats().request_effect_commits,
        request_commits,
        "the move must not commit on the request thread"
    );
    assert_eq!((moved.terminal_revision, moved.replayed, moved.changed), (base + 1, false, true));
    let stats = mux.registry_connection.admit_stats();
    assert_eq!((stats.admitted, stats.fallbacks_connection_held), (1, 0));

    writer_entered.recv_timeout(Duration::from_secs(10)).expect("the writer took the move");
    release.send(()).unwrap();
    let record = mux.workspace_registry.lock().unwrap().terminal_record(TERMINAL).unwrap();
    assert_eq!(record.expect("the terminal row").workspace_key, fixture.to);
    assert_eq!(terminal_revision(mux), base + 1, "the writer committed the replied revision");
    let stats = mux.registry_connection.admit_stats();
    assert_eq!((stats.journal_degraded, stats.unresolved), (0, 0));
    assert_ne!(fixture.from, fixture.to);
}

/// The exit latch: the writer records no move for an exited terminal and
/// answers with the current revision, replayed. The reply says the same, so
/// the journal is not degraded.
#[test]
fn an_exited_terminal_move_replies_with_the_writer_latch_receipt() {
    let fixture = fixture("exited", TerminalLifecycle::Exited);
    let mux = &fixture.mux;
    let base = terminal_revision(mux);

    let (moved, journal_waits) = move_to(&fixture, "move-exited");
    let moved = moved.unwrap();
    assert_eq!(journal_waits, 0);
    assert_eq!(moved.terminal_revision, base);

    assert_eq!(terminal_revision(mux), base, "the read barrier saw the writer's answer");
    let stats = mux.registry_connection.admit_stats();
    assert_eq!((stats.admitted, stats.journal_degraded, stats.unresolved), (1, 0, 0));
}

/// A stale expected revision is refused at prepare: nothing is admitted.
#[test]
fn a_stale_terminal_move_is_refused_before_admission() {
    let fixture = fixture("stale", TerminalLifecycle::Launching);
    let mux = &fixture.mux;
    let base = terminal_revision(mux);
    let error = mux
        .move_terminal_with_mutation(
            TERMINAL,
            &fixture.to,
            None,
            None,
            Some(base + 5),
            &WorkspaceMutation::daemon("move-stale", "test").unwrap(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("terminal revision conflict"), "{error:#}");
    assert_eq!(mux.registry_connection.admit_stats().admitted, 0);
    let record = mux.workspace_registry.lock().unwrap().terminal_record(TERMINAL).unwrap();
    assert_eq!(record.expect("the terminal row").workspace_key, fixture.from);
}
