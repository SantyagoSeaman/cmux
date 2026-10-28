//! p01-process-135: waits, long reads and history reads of a terminal a VT
//! worker process parses. A wait reads the page again when the worker has
//! published (the stream revision), not when output reaches the daemon's
//! tap; a page that is not readable yet is "not matched yet"; a
//! `read-scrollback` holds its surface until the worker answered; and a
//! history read on a path that cannot ask the worker fails instead of
//! answering from the daemon's stale parser.

use super::worker_screen_reads::{request, worker_terminal};
use super::*;
use crate::Surface;
use crate::vt_worker_runtime::fake_screen_source::{FakeScreenSource, text_row, text_screen};
use crate::vt_worker_runtime::{HistoryPage, PageReadError};

/// Waits up to 5 s for `done`.
fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The broker's part once the worker published a parse: advance the
/// terminal's stream revision (no daemon parse, no output tap).
fn broker_saw_publish(surface: &Surface) {
    surface.apply_stream_output_for_test(b"").unwrap();
}

fn connection(mux: &Arc<Mux>) -> (u64, MessageWriter, Arc<BoundedOutbound>) {
    let (writer, outbound) = captured_writer();
    let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());
    (client, writer, outbound)
}

fn terminal_wait(surface: &Surface, timeout_ms: &str) -> String {
    let terminal = surface.terminal_public_id().unwrap().to_string();
    resource_request(
        "wait",
        "terminal.wait",
        json!({"machine": "current", "session": "current", "terminal": terminal,
            "pattern": "READY", "timeout_ms": timeout_ms}),
        None,
    )
}

/// Output reaches the daemon's tap before the worker parsed it: a wait that
/// woke on the tap read the old page and then slept through the publish.
#[test]
fn wait_for_matches_a_page_published_after_the_output_wake() {
    let source = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["booting"], 20))));
    let (mux, surface) = worker_terminal(source.clone());
    let (wait_mux, id) = (mux.clone(), surface.id);
    let waiter = std::thread::spawn(move || {
        cmd_terminal_io::wait_for(&wait_mux, None, id, "READY".into(), 5_000)
    });
    until("the wait read the page", || source.reads() >= 1);
    surface.apply_local_pty_output_for_test(b"READY").unwrap();
    until("the wait read the page after the output", || source.reads() >= 2);

    source.publish(Ok(text_screen(&["READY"], 20)));
    broker_saw_publish(&surface);
    let reply = waiter.join().unwrap().expect("wait-for matched the published page");
    assert_eq!(reply["matched"], true, "{reply}");
    assert_eq!(reply["text"], "READY", "{reply}");
    mux.shutdown();
}

/// A page that outgrew its size is transient: the wait goes on until the
/// worker publishes into the larger page.
#[test]
fn terminal_wait_goes_on_while_the_page_is_resized() {
    let source = Arc::new(FakeScreenSource::scripted(
        [Err(PageReadError::TooSmall { needed: 1 << 20 })],
        Ok(text_screen(&["READY"], 20)),
    ));
    let (mux, surface) = worker_terminal(source.clone());
    let (client, writer, outbound) = connection(&mux);
    let scheduler =
        Arc::new(ConnectionSurfaceScheduler::new(mux.surface_operation_admission.clone()));
    let wait = terminal_wait(&surface, "5000");
    assert!(handle_connection_message(&mux, client, &wait, &writer, &scheduler));
    until("the wait read the resized page", || source.reads() >= 1);
    broker_saw_publish(&surface);

    let reply = pop_json(&outbound);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["result"]["matched"], true, "{reply}");
    assert!(source.bad_pages().is_empty());
    mux.shutdown();
}

/// The router's `terminal.wait` (scripts, apps, the cloud host replica) goes
/// on the same way.
#[test]
fn router_terminal_wait_goes_on_while_the_worker_has_not_published() {
    let source = Arc::new(FakeScreenSource::scripted(
        [Err(PageReadError::NotPublished), Err(PageReadError::Busy)],
        Ok(text_screen(&["READY"], 20)),
    ));
    let (mux, surface) = worker_terminal(source.clone());
    let parsed = crate::resource_router::parse_resource_request_as(
        &terminal_wait(&surface, "5000"),
        crate::Actor::local_user(),
    )
    .unwrap();
    let wait_mux = mux.clone();
    let waiter = std::thread::spawn(move || {
        crate::resource_router::handle_parsed_resource_request(&wait_mux, parsed)
    });
    until("the wait read the unpublished page", || source.reads() >= 1);
    broker_saw_publish(&surface);

    let reply = waiter.join().unwrap().unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["result"]["matched"], true, "{reply}");
    mux.shutdown();
}

/// At the deadline the read is final: a page still not readable fails it.
#[test]
fn terminal_wait_fails_at_the_deadline_on_an_unpublished_page() {
    let source = Arc::new(FakeScreenSource::steady(Err(PageReadError::NotPublished)));
    let (mux, surface) = worker_terminal(source);
    let (client, writer, outbound) = connection(&mux);
    let scheduler =
        Arc::new(ConnectionSurfaceScheduler::new(mux.surface_operation_admission.clone()));
    let wait = terminal_wait(&surface, "20");
    assert!(handle_connection_message(&mux, client, &wait, &writer, &scheduler));
    let reply = pop_json(&outbound);
    assert_eq!(reply["ok"], false, "{reply}");
    mux.shutdown();
}

/// spec/events.md, Ordering Guarantees: later requests of a surface wait
/// for its long read. A worker's `read-scrollback` answers after its run,
/// so the surface stays held until the worker answered.
#[test]
fn read_scrollback_holds_its_surface_until_the_worker_answered() {
    let source = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["now"], 20))));
    let (mux, surface) = worker_terminal(source.clone());
    let (client, writer, outbound) = connection(&mux);
    let scheduler =
        Arc::new(ConnectionSurfaceScheduler::new(mux.surface_operation_admission.clone()));
    let dispatch = |line: Value| {
        let mut pending = Some(serde_json::from_value(line).unwrap());
        assert_eq!(
            scheduler.dispatch(mux.clone(), client, &mut pending, 0, writer.clone()),
            Some(true)
        );
    };
    dispatch(json!({"id": 1, "cmd": "read-scrollback", "surface": surface.id,
        "start": 0, "count": 2}));
    dispatch(json!({"id": 2, "cmd": "read-screen", "surface": surface.id}));

    let deadline = Instant::now() + Duration::from_secs(5);
    let history = loop {
        if let Some(history) = source.take_history_requests().pop() {
            break history;
        }
        assert!(Instant::now() < deadline, "the worker got no history request");
        std::thread::sleep(Duration::from_millis(1));
    };
    // Time for a read-screen that passed the scrollback to answer.
    std::thread::sleep(Duration::from_millis(100));
    let early = outbound.try_pop();
    (history.reply)(Ok(HistoryPage {
        start: 0,
        total: 2,
        epoch: 1,
        rows: vec![text_row("older", 20), text_row("newer", 20)],
    }));
    let replies = [pop_json(&outbound), pop_json(&outbound)];
    let drained = scheduler.close_and_wait(Duration::from_secs(2));

    assert!(early.is_none(), "a request answered before the scrollback it followed: {early:?}");
    assert_eq!([replies[0]["id"].as_u64(), replies[1]["id"].as_u64()], [Some(1), Some(2)]);
    assert_eq!(replies[0]["ok"], true, "{}", replies[0]);
    assert_eq!(replies[1]["data"]["text"], "now", "{}", replies[1]);
    assert!(drained);
    mux.shutdown();
}

/// `copy` scrollback and `terminal.copy` scrollback would read the daemon's
/// stale parser, and the router's `terminal.history.read` cannot wait for
/// the worker: each fails, and none answers from the daemon's parser.
#[test]
fn history_reads_that_cannot_ask_the_worker_fail() {
    let source = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["now"], 20))));
    let (mux, surface) = worker_terminal(source.clone());

    let copy =
        request(&mux, json!({"id": 1, "cmd": "copy", "surface": surface.id, "mode": "scrollback"}));
    assert_eq!(copy["ok"], false, "{copy}");
    assert!(copy["error"].as_str().unwrap().contains("VT worker"), "{copy}");

    let terminal = surface.terminal_public_id().unwrap().to_string();
    let (client, writer, outbound) = connection(&mux);
    let scheduler =
        Arc::new(ConnectionSurfaceScheduler::new(mux.surface_operation_admission.clone()));
    let copy = resource_request(
        "copy",
        "terminal.copy",
        json!({"machine": "current", "session": "current", "terminal": terminal,
            "mode": "scrollback"}),
        None,
    );
    assert!(handle_connection_message(&mux, client, &copy, &writer, &scheduler));
    let copy = pop_json(&outbound);
    assert_eq!(copy["ok"], false, "{copy}");
    assert_eq!(copy["error"]["code"], "operation.failed", "{copy}");

    let history = resource_request(
        "history",
        "terminal.history.read",
        json!({"machine": "current", "session": "current", "terminal": terminal,
            "limit": 2, "styled": false}),
        None,
    );
    let parsed =
        crate::resource_router::parse_resource_request_as(&history, crate::Actor::local_user())
            .unwrap();
    let history = crate::resource_router::handle_parsed_resource_request(&mux, parsed).unwrap();
    assert_eq!(history["ok"], false, "{history}");
    assert!(history["error"]["message"].as_str().unwrap().contains("VT worker"), "{history}");
    assert!(source.take_history_requests().is_empty());
    mux.shutdown();
}
