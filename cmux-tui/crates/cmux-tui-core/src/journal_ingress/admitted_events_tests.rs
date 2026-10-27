//! Admitted frontend events (p05-ipc-126): the reply goes before the
//! journal commit, a bad event is still refused, a cold read waits for it,
//! an I/O failure is retried, and the writer drops an admitted event that
//! fails on its own instead of failing for everyone.

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

fn writer_stats(mux: &Mux) -> crate::diagnostics::JournalWriterSnapshot {
    mux.journal_writer_stats().unwrap()
}

/// A connection of its own to `mux`'s journal database, to inject a write
/// failure with a trigger.
fn injector(root: &std::path::Path) -> rusqlite::Connection {
    let database_path = std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("workspace-registry.sqlite3"))
        .find(|path| path.is_file())
        .expect("persistent journal database");
    rusqlite::Connection::open(database_path).unwrap()
}

/// Admit `event_id` while every `frontend.resized` insert fails (an I/O
/// error stand-in: neither contention nor a content error), and return once
/// the writer paused after the first failure. Send on the returned channel
/// to let it retry; drop the trigger first to let the retry commit.
fn admitted_and_paused_on_a_write_failure(
    mux: &Mux,
    injector: &rusqlite::Connection,
    event_id: &str,
) -> std::sync::mpsc::SyncSender<()> {
    injector
        .execute_batch(
            "CREATE TRIGGER reject_admitted_frontend_event
             BEFORE INSERT ON session_journal
             WHEN NEW.kind = 'frontend.resized'
             BEGIN
               SELECT RAISE(ABORT, 'injected frontend journal write failure');
             END;",
        )
        .unwrap();
    let (failed, failure_observed) = sync_channel(1);
    let (retry, retry_receiver) = sync_channel(1);
    mux.install_journal_nonretryable_failure_hook_for_test(failed, retry_receiver);
    mux.journal_local_frontend_event(resize(event_id, 80)).unwrap();
    failure_observed.recv_timeout(Duration::from_secs(5)).expect("the write did not fail");
    retry
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
    assert_eq!(writer_stats(&mux).admitted_events, 1);
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
    assert_eq!(writer_stats(&mux).admitted_events, 0);
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}

/// Red before the fix (cp/p05-ipc-126 at 13012523624): the writer dropped
/// an admitted event on its first write error. An error that is neither
/// contention nor the event's content gets the bounded retry terminal
/// output gets, and the event commits.
#[test]
fn an_admitted_event_survives_a_short_write_failure() {
    let (mux, root) = persistent("admitted-frontend-retry");
    let injector = injector(&root);
    let retry = admitted_and_paused_on_a_write_failure(&mux, &injector, "event_retried");
    injector.execute_batch("DROP TRIGGER reject_admitted_frontend_event;").unwrap();
    retry.send(()).unwrap();

    assert_eq!(journaled_cols(&mux, "event_retried"), [80]);
    let stats = writer_stats(&mux);
    assert_eq!((stats.admitted_events, stats.journal_degraded), (1, 0));
    drop(injector);
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}

/// Red before the fix: a read right after an `admitted:true` reply ran
/// while the writer still retried the event, and missed it. The read waits
/// for every event admitted before it (read-your-writes), counted.
#[test]
fn a_cold_read_after_an_admitted_reply_waits_for_the_event() {
    let (mux, root) = persistent("admitted-frontend-read-barrier");
    let injector = injector(&root);
    let retry = admitted_and_paused_on_a_write_failure(&mux, &injector, "event_read_back");

    let (read, read_result) = sync_channel(1);
    let reader_mux = mux.clone();
    let reader = std::thread::spawn(move || {
        let _ = read.send(journaled_cols(&reader_mux, "event_read_back"));
    });
    assert!(
        read_result.recv_timeout(Duration::from_millis(300)).is_err(),
        "the read went ahead of an event admitted before it"
    );
    injector.execute_batch("DROP TRIGGER reject_admitted_frontend_event;").unwrap();
    retry.send(()).unwrap();
    let cols = read_result.recv_timeout(Duration::from_secs(5)).expect("the read never ended");
    reader.join().unwrap();

    assert_eq!(cols, [80], "the read did not see the event its reply admitted");
    let stats = writer_stats(&mux);
    assert!(stats.read_barrier_waits >= 1, "the wait was not counted");
    assert_eq!(stats.read_barrier_timeouts, 0);
    // Nothing is pending now: the next read does not wait.
    assert_eq!(journaled_cols(&mux, "event_read_back"), [80]);
    assert_eq!(writer_stats(&mux).read_barrier_waits, stats.read_barrier_waits);
    drop(injector);
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}

/// Red before the fix: an admitted event's batch that failed (not
/// contention) after its deadline stopped the writer, and every later
/// journal write failed until restart. Here the commit-admission hook
/// holds the batch past its deadline, then fails it.
#[test]
fn an_admitted_event_failing_past_its_deadline_does_not_stop_the_writer() {
    let (mux, root) = persistent("admitted-frontend-past-deadline");
    let (failed, writer_failure) = sync_channel(1);
    mux.install_journal_failure_notifier_for_test(failed);
    let (entered, commit_entered) = sync_channel(1);
    let (release, release_receiver) = sync_channel::<()>(1);
    mux.install_journal_after_commit_admission_for_test(entered, release_receiver);
    mux.journal_local_frontend_event(resize("event_past_deadline", 80)).unwrap();
    commit_entered.recv_timeout(Duration::from_secs(5)).unwrap();
    std::thread::sleep(super::super::JOURNAL_DURABLE_WAIT + Duration::from_millis(200));
    // The hook's release closes: the commit fails past the batch deadline.
    drop(release);

    mux.journal_frontend_event(
        "client_after_the_deadline".into(),
        resize("event_after_the_deadline", 100),
        FrontendJournalReply::AfterCommit,
    )
    .expect("the writer stopped for an admitted event's failure");
    assert!(writer_failure.try_recv().is_err(), "the journal writer failed permanently");
    // The admitted event itself was retried and committed.
    assert_eq!(journaled_cols(&mux, "event_past_deadline"), [80]);
    assert_eq!(journaled_cols(&mux, "event_after_the_deadline"), [100]);
    assert_eq!(writer_stats(&mux).journal_degraded, 0);
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

    let stats = writer_stats(&mux);
    assert_eq!((stats.admitted_events, stats.journal_degraded), (2, 1));
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
    assert_eq!(writer_stats(&mux).admitted_events, 0);
    drop(mux);
    std::fs::remove_dir_all(root).unwrap();
}
