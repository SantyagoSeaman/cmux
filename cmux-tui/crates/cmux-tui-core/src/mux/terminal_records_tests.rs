//! Daemon-local terminal rows are admitted, not waited for (F10,
//! p05-ipc-122): the journal writer commits them in admit order, a tombstone
//! after the rows admitted before it, and what the writer would refuse or
//! latch is decided before admission.

use std::path::PathBuf;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use super::*;
use crate::request_path::{BlockingKind, RequestPathMode, RequestPathScope, RequestPathStats};

/// The writer delay the F10 acceptance injects.
const WRITER_DELAY: Duration = Duration::from_millis(50);

fn persistent_mux(session: &str) -> (Arc<Mux>, PathBuf) {
    let root = std::env::temp_dir()
        .join(format!("cmux-terminal-records-{session}-{}", WorkspacePublicId::random().unwrap()));
    let registry = WorkspaceRegistry::open(&root, session).unwrap();
    let mux = Mux::from_workspace_registry(
        session.into(),
        SurfaceOptions::default(),
        registry,
        ProviderWorkspaceState::default(),
        true,
    )
    .unwrap();
    (mux, root)
}

fn launching_terminal(workspace_key: &str) -> RegistryTerminal {
    RegistryTerminal {
        terminal_id: TerminalId::random().unwrap().to_hex(),
        workspace_key: workspace_key.to_string(),
        incarnation: None,
        lifecycle: TerminalLifecycle::Launching,
        launch_spec: serde_json::json!({}),
        exit: None,
        on_exit: TerminalOnExit::Close,
    }
}

#[test]
fn a_tombstone_lands_after_the_rows_admitted_before_it() {
    let (mux, root) = persistent_mux("tombstone-order");
    let workspace = mux.create_empty_workspace(Some("order".into()), None, None).unwrap();
    let mut terminal = launching_terminal(&workspace.key);

    // The writer's next batch stops before it commits for WRITER_DELAY.
    let (entered, writer_entered) = sync_channel(1);
    let (release_writer, release) = sync_channel(1);
    mux.install_journal_before_commit_for_test(entered, release);
    let releaser = std::thread::spawn(move || {
        if writer_entered.recv_timeout(Duration::from_secs(10)).is_ok() {
            std::thread::sleep(WRITER_DELAY);
        }
        let _ = release_writer.send(());
    });

    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    let (reserved, tombstoned) = {
        let _scope = RequestPathScope::enter(&stats);
        let mut registry = mux.workspace_registry.lock().unwrap();
        let reserved = commit_terminal_transition(
            &mut registry,
            "terminal-reserved",
            "reserve-terminal",
            &terminal,
        )
        .unwrap();
        terminal.lifecycle = TerminalLifecycle::Tombstoned;
        let tombstoned = commit_terminal_transition(
            &mut registry,
            "terminal-closed",
            "close-terminal",
            &terminal,
        )
        .unwrap();
        (reserved, tombstoned)
    };
    releaser.join().unwrap();

    assert_eq!(
        stats.count(BlockingKind::JournalWait),
        0,
        "both rows must be admitted, not waited for: {:?}",
        stats.sites()
    );
    assert_eq!(tombstoned, reserved + 1, "the tombstone takes the next admitted revision");
    let registry = mux.workspace_registry.lock().unwrap();
    let events = registry
        .terminal_events_after(reserved - 1)
        .unwrap()
        .into_iter()
        .filter(|event| event.terminal_id == terminal.terminal_id)
        .map(|event| (event.revision, event.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        [(reserved, "terminal-reserved".to_string()), (tombstoned, "terminal-closed".to_string())],
        "the writer commits the rows in admit order"
    );
    let record = registry.terminal_record(&terminal.terminal_id).unwrap().unwrap();
    assert_eq!(record.lifecycle, TerminalLifecycle::Tombstoned);
    drop(registry);
    drop(mux);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_lifecycle_after_a_tombstone_is_refused_before_admission() {
    let (mux, root) = persistent_mux("after-tombstone");
    let workspace = mux.create_empty_workspace(Some("closed".into()), None, None).unwrap();
    let mut terminal = launching_terminal(&workspace.key);
    let mut registry = mux.workspace_registry.lock().unwrap();
    commit_terminal_transition(&mut registry, "terminal-reserved", "reserve-terminal", &terminal)
        .unwrap();
    terminal.lifecycle = TerminalLifecycle::Tombstoned;
    let tombstoned =
        commit_terminal_transition(&mut registry, "terminal-closed", "close-terminal", &terminal)
            .unwrap();

    let error = commit_terminal_lifecycle(
        &mut registry,
        "terminal-ready",
        "terminal-ready",
        &terminal.terminal_id,
        TerminalLifecycle::Running,
        Some("1000000000004000800000000000000d"),
        None,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("tombstoned terminal id cannot be reused"), "{error:#}");
    assert_eq!(registry.terminal_revision().unwrap(), tombstoned, "nothing was admitted");
    drop(registry);
    drop(mux);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_second_exit_keeps_the_first_and_admits_no_revision() {
    const INCARNATION: &str = "1000000000004000800000000000000e";
    let (mux, root) = persistent_mux("exit-latch");
    let workspace = mux.create_empty_workspace(Some("exit".into()), None, None).unwrap();
    let terminal = launching_terminal(&workspace.key);
    let mut registry = mux.workspace_registry.lock().unwrap();
    commit_terminal_transition(&mut registry, "terminal-reserved", "reserve-terminal", &terminal)
        .unwrap();
    let receipt = |code: i32| {
        serde_json::json!({
            "outcome": crate::terminal_host_protocol::TerminalExitOutcome::Exit { code },
            "exited_at": "12",
            "revision": "1",
        })
    };
    let mut exit = |code: i32| {
        commit_terminal_lifecycle(
            &mut registry,
            "terminal-exited",
            "terminal-host-exited",
            &terminal.terminal_id,
            TerminalLifecycle::Exited,
            Some(INCARNATION),
            Some(receipt(code)),
        )
    };
    let (_, first) = exit(1).unwrap();
    let (latched, second) = exit(2).unwrap();
    assert_eq!(second, first, "a second exit mints no revision");
    assert_eq!(latched.exit, Some(receipt(1)), "the first exit stays");
    assert_eq!(registry.terminal_revision().unwrap(), first);

    let error = commit_terminal_lifecycle(
        &mut registry,
        "terminal-exited",
        "terminal-host-exited",
        &terminal.terminal_id,
        TerminalLifecycle::Exited,
        Some("2000000000004000800000000000000e"),
        None,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("terminal_incarnation_mismatch"), "{error:#}");
    drop(registry);
    drop(mux);
    let _ = std::fs::remove_dir_all(root);
}
