//! `journal-frontend-event`'s reply per client (p05-ipc-126): a client that
//! negotiated `frontend-journal-v2` reads `admitted`; every other client
//! keeps protocol 10's `committed:true`, given after the commit.

use super::super::*;

fn writer() -> MessageWriter {
    MessageWriter::new(QueuedSink { outbound: Arc::new(BoundedOutbound::default()), control: None })
}

fn persistent(name: &str) -> (Arc<Mux>, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "cmux-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let mux = Mux::open_persistent(name, crate::SurfaceOptions::default(), &root).unwrap();
    (mux, root)
}

fn client(mux: &Arc<Mux>, capabilities: &[&str]) -> u64 {
    let client = mux.control_clients.register(ClientTransport::Unix, writer());
    let capabilities = capabilities.iter().map(|name| (*name).to_string()).collect();
    mux.control_clients.set_info(client, None, None, Some(capabilities)).unwrap();
    client
}

fn resize(event_id: &str) -> crate::FrontendJournalEvent {
    crate::FrontendJournalEvent::Resize {
        event_id: event_id.into(),
        frontend_projection_id: crate::resource::FrontendProjectionPublicId::parse(format!(
            "projection_{:032x}",
            10
        ))
        .unwrap(),
        generation: "frontend_generation_1".into(),
        cols: 80,
        rows: 24,
        cell_width: 8,
        cell_height: 16,
    }
}

fn journaled(mux: &Mux, event_id: &str) -> bool {
    mux.session_journal_after(0, 1024)
        .unwrap()
        .records
        .iter()
        .any(|record| record.event_id == event_id)
}

#[test]
fn only_a_frontend_journal_v2_client_reads_admitted() {
    let (mux, root) = persistent("frontend-journal-v2-reply");
    mux.set_admit_then_reply_for_test(true);
    let old = client(&mux, &[FRONTEND_JOURNAL_CAPABILITY]);
    let v2 = client(&mux, &[FRONTEND_JOURNAL_CAPABILITY, FRONTEND_JOURNAL_ADMITTED_CAPABILITY]);
    assert!(mux.control_clients.supports_capability(v2, FRONTEND_JOURNAL_ADMITTED_CAPABILITY));

    // An old client's committed:true still means the journal has it.
    let reply = super::journal_frontend_event(&mux, old, resize("event_old_client")).unwrap();
    assert_eq!(reply, json!({"committed":true}));
    assert!(journaled(&mux, "event_old_client"));
    assert_eq!(mux.journal_writer_stats().unwrap().admitted_events, 0);

    let reply = super::journal_frontend_event(&mux, v2, resize("event_v2_client")).unwrap();
    assert_eq!(reply, json!({"committed":true,"admitted":true}));
    assert_eq!(mux.journal_writer_stats().unwrap().admitted_events, 1);

    // The F10 setting off: a v2 client waits and reads admitted:false.
    mux.set_admit_then_reply_for_test(false);
    let reply = super::journal_frontend_event(&mux, v2, resize("event_v2_setting_off")).unwrap();
    assert_eq!(reply, json!({"committed":true,"admitted":false}));
    assert!(journaled(&mux, "event_v2_setting_off"));
    // The admitted event is ahead of it on the lane.
    assert!(journaled(&mux, "event_v2_client"));
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}

/// The task's acceptance (p05-ipc.md, 121..126): an admitted
/// `journal-frontend-event` notes no `journal_wait` on its request path; a
/// client without `frontend-journal-v2` still waits for the commit.
#[test]
fn an_admitted_journal_frontend_event_notes_no_journal_wait() {
    use crate::request_path::{
        RequestPathIoSnapshot, RequestPathMode, RequestPathScope, RequestPathStats,
    };
    let (mux, root) = persistent("frontend-journal-v2-no-wait");
    mux.set_admit_then_reply_for_test(true);
    let old = client(&mux, &[FRONTEND_JOURNAL_CAPABILITY]);
    let v2 = client(&mux, &[FRONTEND_JOURNAL_CAPABILITY, FRONTEND_JOURNAL_ADMITTED_CAPABILITY]);
    let journal_waits = |client: u64, event_id: &str| {
        let stats = RequestPathStats::new(RequestPathMode::RECORD);
        let _scope = RequestPathScope::enter(&stats);
        super::journal_frontend_event(&mux, client, resize(event_id)).unwrap();
        RequestPathIoSnapshot::of(&stats).journal_wait
    };

    assert_eq!(journal_waits(v2, "event_v2_no_wait"), 0);
    assert_eq!(journal_waits(old, "event_old_waits"), 1);
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn identify_advertises_frontend_journal_v2() {
    assert!(build_capabilities().contains(&FRONTEND_JOURNAL_ADMITTED_CAPABILITY));
}
