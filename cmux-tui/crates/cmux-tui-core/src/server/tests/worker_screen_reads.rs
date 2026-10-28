//! p01-process-135: a terminal a VT worker process parses answers screen
//! reads from the worker's page, with no request to the worker and no read
//! of the daemon's parser, and `read-scrollback` from the worker's answer,
//! never by waiting on the request thread.

use super::*;
use crate::Surface;
use crate::mux::mux_work::on_request_thread_in_panic_mode;
use crate::vt_worker_runtime::fake_screen_source::{
    FakeScreenSource, parsed_terminal, published_screen, text_row, text_screen,
};
use crate::vt_worker_runtime::{HistoryPage, HistoryRange, PageReadError};

/// A mux whose first terminal's screen comes from `source`; the daemon's
/// own parser for it holds other text, so a read that does not use the page
/// shows.
pub(super) fn worker_terminal(source: Arc<FakeScreenSource>) -> (Arc<Mux>, Arc<Surface>) {
    let mux = test_mux();
    let surface = mux.new_workspace(None, Some((20, 4))).unwrap();
    surface.apply_stream_output_for_test(b"daemon parser").unwrap();
    mux.worker_screens().attach(surface.id, source);
    (mux, surface)
}

pub(super) fn request(mux: &Arc<Mux>, request: Value) -> Value {
    let (writer, outbound) = captured_writer();
    let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());
    assert!(handle_message(mux, client, &request.to_string(), &writer));
    pop_json(&outbound)
}

#[test]
fn read_screen_wait_for_and_copy_read_the_worker_page() {
    let source = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["from the page"], 20))));
    let (mux, surface) = worker_terminal(source.clone());

    let screen = request(&mux, json!({"id": 1, "cmd": "read-screen", "surface": surface.id}));
    assert_eq!(screen["data"]["text"], "from the page", "{screen}");
    let wait = json!({"id": 2, "cmd": "wait-for", "surface": surface.id,
        "pattern": "the page", "timeout_ms": 0});
    let wait = request(&mux, wait);
    assert_eq!(wait["data"]["matched"], true, "{wait}");
    let copy =
        request(&mux, json!({"id": 3, "cmd": "copy", "surface": surface.id, "mode": "screen"}));
    assert_eq!(copy["data"]["text"], "from the page", "{copy}");
    assert_eq!(source.reads(), 3);

    // Without the worker the daemon's parser answers again.
    assert!(mux.worker_screens().detach(surface.id));
    let screen = request(&mux, json!({"id": 4, "cmd": "read-screen", "surface": surface.id}));
    assert_eq!(screen["data"]["text"], "daemon parser", "{screen}");
    mux.shutdown();
}

/// `terminal.screen.read` of a worker page answers what the daemon's parser
/// answers for the same output, the cursor position included while the
/// program hides the cursor (DECTCEM off, as vim, htop and agent TUIs do).
#[test]
fn terminal_screen_read_of_a_worker_page_matches_the_daemon_with_the_cursor_hidden() {
    let bytes = b"agent prompt\r\n> \x1b[?25l";
    let mux = test_mux();
    let surface = mux.new_workspace(None, Some((20, 4))).unwrap();
    surface.apply_stream_output_for_test(bytes).unwrap();
    let daemon = surface.terminal_screen_snapshot().unwrap();
    assert_eq!((daemon.cursor_col, daemon.cursor_row, daemon.cursor_visible), (2, 1, false));
    let mut worker = parsed_terminal(daemon.cols, daemon.rows, bytes);
    let source = Arc::new(FakeScreenSource::steady(Ok(published_screen(&mut worker))));
    mux.worker_screens().attach(surface.id, source.clone());

    let (writer, outbound) = captured_writer();
    let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());
    let scheduler =
        Arc::new(ConnectionSurfaceScheduler::new(mux.surface_operation_admission.clone()));
    let terminal = surface.terminal_public_id().unwrap().to_string();
    let read = resource_request(
        "screen",
        "terminal.screen.read",
        json!({"machine": "current", "session": "current", "terminal": terminal}),
        None,
    );
    assert!(handle_connection_message(&mux, client, &read, &writer, &scheduler));

    let reply = pop_json(&outbound);
    assert_eq!(reply["ok"], true, "{reply}");
    let result = &reply["result"];
    assert_eq!(source.reads(), 1, "the read did not use the worker page");
    assert_eq!(result["text"], daemon.text.as_str(), "{reply}");
    assert_eq!(
        (result["cols"].as_u64(), result["rows"].as_u64()),
        (Some(u64::from(daemon.cols)), Some(u64::from(daemon.rows)))
    );
    assert_eq!(
        (result["cursor_col"].as_u64(), result["cursor_row"].as_u64()),
        (Some(u64::from(daemon.cursor_col)), Some(u64::from(daemon.cursor_row))),
        "{reply}"
    );
    assert_eq!(result["cursor_visible"], daemon.cursor_visible);
    mux.shutdown();
}

#[test]
fn a_bad_page_fails_the_read_and_ends_the_worker() {
    let source = Arc::new(FakeScreenSource::steady(Err(PageReadError::Corrupt("magic"))));
    let (mux, surface) = worker_terminal(source.clone());
    let screen = request(&mux, json!({"id": 1, "cmd": "read-screen", "surface": surface.id}));
    assert_eq!(screen["ok"], false, "{screen}");
    assert!(screen["error"].as_str().unwrap().contains("bad terminal screen page"), "{screen}");
    assert_eq!(source.bad_pages(), ["magic"]);
    mux.shutdown();
}

#[test]
fn read_scrollback_answers_from_the_worker_off_the_request_thread() {
    let source = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["now"], 20))));
    let (mux, surface) = worker_terminal(source.clone());
    let (writer, outbound) = captured_writer();
    let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());

    let request_mux = mux.clone();
    let id = surface.id;
    let answered = on_request_thread_in_panic_mode(move || {
        let request =
            json!({"id": 5, "cmd": "read-scrollback", "surface": id, "start": 10, "count": 2});
        handle_message(&request_mux, client, &request.to_string(), &writer)
    });
    assert!(answered.is_ok(), "read-scrollback waited for the worker on its request thread");
    assert!(outbound.try_pop().is_none(), "the reply waits for the worker's rows");

    let pending = source.take_history_requests();
    assert_eq!(pending.len(), 1);
    let history = pending.into_iter().next().unwrap();
    assert_eq!(history.range, HistoryRange::From { start: 10, count: 2 });
    (history.reply)(Ok(HistoryPage {
        start: 10,
        total: 40,
        epoch: 7,
        rows: vec![text_row("older", 20), text_row("newer", 20)],
    }));

    let reply = pop_json(&outbound);
    assert_eq!(reply["id"], 5);
    assert_eq!(reply["ok"], true, "{reply}");
    let data = &reply["data"];
    assert_eq!((data["start"].as_u64(), data["total"].as_u64()), (Some(10), Some(40)));
    assert_eq!(data["epoch"], 7);
    let rows = data["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["runs"][0]["text"].as_str().map(str::trim_end), Some("older"), "{reply}");
    mux.shutdown();
}

#[test]
fn a_bad_scrollback_answer_fails_the_request_and_ends_the_worker() {
    let source = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["now"], 20))));
    let (mux, surface) = worker_terminal(source.clone());
    let (writer, outbound) = captured_writer();
    let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());
    let request = json!({"id": 6, "cmd": "read-scrollback", "surface": surface.id,
        "start": 0, "count": 1});
    assert!(handle_message(&mux, client, &request.to_string(), &writer));
    let history = source.take_history_requests().into_iter().next().unwrap();
    (history.reply)(Ok(HistoryPage {
        start: 0,
        total: 1,
        epoch: 0,
        rows: vec![text_row("a", 20), text_row("b", 20)],
    }));

    let reply = pop_json(&outbound);
    assert_eq!(reply["ok"], false, "{reply}");
    assert_eq!(source.bad_pages(), ["more history rows than asked"]);
    mux.shutdown();
}

#[test]
fn read_scrollback_of_a_daemon_parsed_terminal_stays_synchronous() {
    let source = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["now"], 20))));
    let (mux, surface) = worker_terminal(source.clone());
    assert!(mux.worker_screens().detach(surface.id));
    let reply = request(
        &mux,
        json!({"id": 7, "cmd": "read-scrollback", "surface": surface.id, "start": 0, "count": 5}),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    assert!(source.take_history_requests().is_empty());
    mux.shutdown();
}

#[test]
fn terminal_history_read_answers_from_the_worker_off_the_request_thread() {
    let source = Arc::new(FakeScreenSource::steady(Ok(text_screen(&["now"], 20))));
    let (mux, surface) = worker_terminal(source.clone());
    let (writer, outbound) = captured_writer();
    let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());
    let scheduler =
        Arc::new(ConnectionSurfaceScheduler::new(mux.surface_operation_admission.clone()));
    let terminal = surface.terminal_public_id().unwrap().to_string();

    let request_mux = mux.clone();
    let answered = on_request_thread_in_panic_mode(move || {
        let read = resource_request(
            "history",
            "terminal.history.read",
            json!({"machine": "current", "session": "current", "terminal": terminal,
                "before": "30", "limit": 2, "styled": false}),
            None,
        );
        handle_connection_message(&request_mux, client, &read, &writer, &scheduler)
    });
    assert!(answered.is_ok(), "terminal.history.read waited for the worker on its request thread");
    assert!(outbound.try_pop().is_none(), "the result waits for the worker's rows");

    let history = source.take_history_requests().into_iter().next().unwrap();
    assert_eq!(history.range, HistoryRange::Before { before: Some(30), limit: 2 });
    (history.reply)(Ok(HistoryPage {
        start: 28,
        total: 40,
        epoch: 1,
        rows: vec![text_row("twenty-eight", 20), text_row("twenty-nine", 20)],
    }));

    let reply = pop_json(&outbound);
    assert_eq!(reply["id"], "history");
    assert_eq!(reply["ok"], true, "{reply}");
    let result = &reply["result"];
    assert_eq!((result["start"].as_str(), result["next"].as_str()), (Some("28"), Some("28")));
    let rows = result["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["runs"][0]["text"].as_str().map(str::trim_end), Some("twenty-nine"));
    mux.shutdown();
}
