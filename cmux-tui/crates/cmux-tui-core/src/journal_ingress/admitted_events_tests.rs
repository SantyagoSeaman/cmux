//! Admitted frontend events (p05-ipc-126): the reply goes before the
//! journal commit, a bad event is still refused, and the writer drops an
//! admitted event that fails on its own instead of failing for everyone.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use crate::mux::FrontendJournalReply;
use crate::resource::FrontendProjectionPublicId;
use crate::{FrontendJournalEvent, Mux};

fn persistent(name: &str) -> (Arc<Mux>, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "cmux-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let mux = Mux::open_persistent(name, crate::SurfaceOptions::default(), &root).unwrap();
    mux.set_admit_then_reply_for_test(true);
    (mux, root)
}

fn resize(event_id: &str, cols: u16) -> FrontendJournalEvent {
    FrontendJournalEvent::Resize {
        event_id: event_id.into(),
        frontend_projection_id: FrontendProjectionPublicId::parse(format!("projection_{:032x}", 9))
            .unwrap(),
        generation: "frontend_generation_1".into(),
        cols,
        rows: 24,
        cell_width: 8,
        cell_height: 16,
    }
}

fn journaled_cols(mux: &Mux, event_id: &str) -> Vec<u64> {
    mux.session_journal_after(0, 1024)
        .unwrap()
        .records
        .iter()
        .filter(|record| record.event_id == event_id)
        .map(|record| record.payload["cols"].as_u64().unwrap())
        .collect()
}

fn receipt_waits(mux: &Mux) -> u64 {
    mux.journal_writer_stats().unwrap().receipt_wait_us.count
}

/// Red on the base: the event waited for the writer, which cannot commit
/// while the registry connection is held, so no reply came within 1 s.
#[test]
fn an_admitted_frontend_event_replies_before_the_writer_commits_it() {
    let (mux, root) = persistent("admitted-frontend-reply");
    let (entered, entered_receiver) = sync_channel(1);
    let (release, release_receiver) = sync_channel(1);
    let holder_mux = mux.clone();
    let holder = std::thread::spawn(move || {
        holder_mux.hold_registry_connection_for_test(entered, release_receiver);
    });
    entered_receiver.recv().unwrap();
    let waits = receipt_waits(&mux);
    let epoch = mux.journal_event_epoch();

    let (replied, reply) = sync_channel(1);
    let caller_mux = mux.clone();
    let caller = std::thread::spawn(move || {
        let _ = replied.send(caller_mux.journal_local_frontend_event(resize("event_admitted", 80)));
    });
    let reply = reply.recv_timeout(Duration::from_secs(1));
    release.send(()).unwrap();
    holder.join().unwrap();
    caller.join().unwrap();

    reply.expect("the reply waited for the journal writer").unwrap();
    assert_eq!(receipt_waits(&mux), waits, "an admitted event waited for its receipt");
    assert_eq!(mux.admitted_journal_event_stats().admitted, 1);
    // The writer commits it once the connection is free.
    assert_ne!(mux.wait_for_journal_event(epoch, Duration::from_secs(10)), epoch);
    assert_eq!(journaled_cols(&mux, "event_admitted"), [80]);
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_bad_admitted_frontend_event_is_refused_before_its_reply() {
    let (mux, root) = persistent("admitted-frontend-refused");
    let error = mux.journal_local_frontend_event(resize("event_zero_cols", 0)).unwrap_err();
    assert!(error.to_string().contains("geometry must be positive"), "{error:#}");
    let error = mux.journal_local_frontend_event(resize(" ", 80)).unwrap_err();
    assert!(error.to_string().contains("event id cannot be empty"), "{error:#}");
    assert_eq!(mux.admitted_journal_event_stats().admitted, 0);
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}

/// An event id reused with different content is the one content error the
/// writer finds. The admitted copy is dropped and counted; the writer keeps
/// committing later events (a terminal-lane failure would fail it).
#[test]
fn an_admitted_event_that_fails_alone_is_dropped_and_the_writer_keeps_going() {
    let (mux, root) = persistent("admitted-frontend-degraded");
    mux.journal_local_frontend_event(resize("event_reused", 80)).unwrap();
    mux.journal_local_frontend_event(resize("event_reused", 81)).unwrap();
    let principal = "client_admitted_degraded".to_string();
    let after = mux
        .journal_frontend_event(
            principal.clone(),
            resize("event_after", 100),
            FrontendJournalReply::AfterCommit,
        )
        .unwrap();
    assert_eq!(after, FrontendJournalReply::AfterCommit);

    let stats = mux.admitted_journal_event_stats();
    assert_eq!((stats.admitted, stats.degraded), (2, 1));
    assert_eq!(journaled_cols(&mux, "event_reused"), [80]);
    assert_eq!(journaled_cols(&mux, "event_after"), [100]);
    mux.journal_frontend_event(
        principal,
        resize("event_later", 120),
        FrontendJournalReply::AfterCommit,
    )
    .expect("the writer failed after an admitted event's own failure");
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn with_the_setting_off_an_admitted_request_waits_for_the_commit() {
    let (mux, root) = persistent("admitted-frontend-off");
    mux.set_admit_then_reply_for_test(false);
    let waits = receipt_waits(&mux);
    let reply = mux
        .journal_frontend_event(
            "client_admit_off".into(),
            resize("event_off", 80),
            FrontendJournalReply::AfterAdmit,
        )
        .unwrap();
    assert_eq!(reply, FrontendJournalReply::AfterCommit);
    assert_eq!(receipt_waits(&mux), waits + 1);
    assert_eq!(journaled_cols(&mux, "event_off"), [80]);
    assert_eq!(mux.admitted_journal_event_stats().admitted, 0);
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}
