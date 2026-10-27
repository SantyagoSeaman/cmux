//! A reserved terminal creation (split, new-tab) admits its
//! `terminal-reserved` row and moves on: no journal writer receipt wait on
//! the request path (F10, p05-ipc-121).

use std::sync::mpsc::sync_channel;
use std::time::Duration;

use super::*;
use crate::request_path::{BlockingKind, RequestPathMode, RequestPathScope, RequestPathStats};

/// The writer delay the F10 acceptance injects.
const WRITER_DELAY: Duration = Duration::from_millis(50);

#[test]
fn reserved_terminal_admits_its_reservation_without_a_journal_wait() {
    let root = std::env::temp_dir()
        .join(format!("cmux-surface-spawn-admit-{}", WorkspacePublicId::random().unwrap()));
    let session = "surface-spawn-admit";
    let registry = WorkspaceRegistry::open(&root, session).unwrap();
    let mux = Mux::from_workspace_registry(
        session.into(),
        SurfaceOptions::default(),
        registry,
        ProviderWorkspaceState::default(),
        true,
    )
    .unwrap();
    let workspace = mux.create_empty_workspace(Some("admit".into()), None, None).unwrap();

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
    let (reserved, waits_at_reservation) = sync_channel(1);
    mux.set_resource_terminal_reservation_hook_for_test(Some(Arc::new({
        let stats = stats.clone();
        move |_terminal_id: &str| {
            let _ = reserved.try_send(stats.count(BlockingKind::JournalWait));
        }
    })));
    let terminal_id = TerminalId::random().unwrap();
    let terminal_hex = terminal_id.to_hex();
    let reservation = TerminalReservationRequest {
        terminal_id,
        mutation: WorkspaceMutation::daemon("surface-spawn-admit", "test").unwrap(),
        fingerprint: serde_json::json!({"test":"surface-spawn-admit"}),
        expected_generation: None,
        expected_revision: None,
        on_exit: TerminalOnExit::Close,
        tab_id: None,
        env: Vec::new(),
    };
    let spawned = {
        let _scope = RequestPathScope::enter(&stats);
        mux.spawn_surface_in_workspace_reserved(
            &workspace.key,
            None,
            Some((80, 24)),
            None,
            reservation,
        )
    };
    mux.set_resource_terminal_reservation_hook_for_test(None);
    releaser.join().unwrap();

    let surface = spawned.expect("the reserved terminal spawns");
    assert_eq!(
        waits_at_reservation.try_recv().expect("the reservation hook ran"),
        0,
        "the terminal-reserved row must be admitted, not waited for: {:?}",
        stats.sites()
    );
    let record = mux
        .workspace_registry
        .lock()
        .unwrap()
        .terminal_record(&terminal_hex)
        .unwrap()
        .expect("the writer committed the admitted reservation");
    assert_eq!(record.workspace_key, workspace.key);
    surface.kill();
    drop(mux);
    let _ = std::fs::remove_dir_all(root);
}
