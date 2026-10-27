//! `subscribe {stream}` on a streamed page relay (p05-ipc-148,
//! plans/cmux-next/request-origin.md "Page streams"): a page stream hears
//! only its page's events, each tagged with the stream, and nothing of a
//! closed or replaced subscription follows the reply that ended it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::page_stream_tests::{Line, mux};
use crate::event_bus::MuxEventKind;
use crate::server::*;

const WAIT: Duration = Duration::from_secs(5);
/// How long a test waits to be sure a line does not come.
const QUIET: Duration = Duration::from_millis(200);

fn history_changed(revision: u64) -> MuxEvent {
    MuxEvent::HistoryChanged { revision, kinds: vec!["visit".to_owned()] }
}

/// Every kind of event a busy session emits, then one `history-changed`.
fn emit_busy_session(mux: &Mux, revision: u64) {
    for surface in 0..200 {
        mux.emit(MuxEvent::SurfaceOutput(surface % 16));
    }
    mux.emit(MuxEvent::Bell(1));
    mux.emit(MuxEvent::Status("busy".to_owned()));
    mux.emit(MuxEvent::TitleChanged { surface: 1, title: "t".into() });
    mux.emit(MuxEvent::TreeChanged);
    mux.emit(MuxEvent::PersonalChanged { personal_revision: 3 });
    mux.emit(history_changed(revision));
}

fn subscribe(stream: u16) -> Value {
    json!({"id": 30, "cmd": "subscribe", "stream": stream})
}

fn refusal_reason(reply: &Value) -> (&str, &str) {
    assert_eq!(reply["ok"], false, "{reply}");
    let code = reply["error_code"].as_str().unwrap_or_default();
    (code, reply["error_details"]["reason"].as_str().unwrap_or_default())
}

#[test]
fn a_page_stream_hears_only_its_page_tagged_with_its_stream() {
    let mux = mux("subscribe-only-its-page");
    let mut relay = Line::streamed_relay(&mux);
    relay.open(&mux, 1, "history");
    relay.open(&mux, 2, "apps");

    let reply = relay.send(&mux, &subscribe(1));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["data"]["stream"], 1, "{reply}");
    // The Apps page hears no events: its stream gets no subscription.
    let reply = relay.send(&mux, &subscribe(2));
    assert_eq!(refusal_reason(&reply), ("origin.forbidden", "page_events"));

    emit_busy_session(&mux, 7);
    let line = relay.next_line(WAIT).expect("the history page's event");
    assert_eq!(
        line,
        json!({"stream": 1, "event": "history-changed", "revision": 7, "kinds": ["visit"]})
    );
    assert_eq!(
        relay.next_line(QUIET),
        None,
        "the page heard another page's or the session's event"
    );
    // The bus never queued the session's output for the page stream.
    let probe = mux.control_clients.subscribe_probe(relay.client);
    assert_eq!(probe.received.of(MuxEventKind::SurfaceOutput), 0);
    assert_eq!(probe.received.of(MuxEventKind::Bell), 0);
    mux.shutdown();
}

#[test]
fn subscribing_a_stream_that_is_not_open_is_refused() {
    let mux = mux("subscribe-not-open");
    let mut relay = Line::streamed_relay(&mux);
    let reply = relay.send(&mux, &subscribe(3));
    assert_eq!(refusal_reason(&reply), ("origin.forbidden", "stream_not_open"));
    relay.open(&mux, 3, "history");
    let closed = relay.send(&mux, &json!({"id": 34, "cmd": "close-stream", "stream": 3}));
    assert_eq!(closed["data"]["closed"], true, "{closed}");
    let reply = relay.send(&mux, &subscribe(3));
    assert_eq!(refusal_reason(&reply), ("origin.forbidden", "stream_not_open"));
    for (request, field) in [
        (json!({"id": 31, "cmd": "subscribe", "stream": 0}), "stream"),
        (
            json!({"id": 32, "cmd": "subscribe", "stream": 3, "tree_events": "deltas"}),
            "tree_events",
        ),
        (json!({"id": 33, "cmd": "subscribe", "stream": 3, "surface": 1}), "surface"),
    ] {
        let reply = relay.send(&mux, &request);
        assert_eq!(reply["error_code"], "page_stream.bad_request", "{reply}");
        assert_eq!(reply["error_details"]["field"], field, "{reply}");
    }
    mux.shutdown();
}

/// While the page's events stream in, `close-stream` ends the stream's
/// subscription: no line of the stream follows the close reply, and the
/// id reopened for another page hears nothing of the old one.
#[test]
fn nothing_of_a_closed_stream_follows_its_close_reply() {
    let mux = mux("subscribe-close");
    let mut relay = Line::streamed_relay(&mux);
    relay.open(&mux, 1, "history");
    assert_eq!(relay.send(&mux, &subscribe(1))["ok"], true);

    let stop = Arc::new(AtomicBool::new(false));
    let emitted = Arc::new(AtomicU64::new(0));
    let emitter = {
        let (mux, stop, emitted) = (mux.clone(), stop.clone(), emitted.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let revision = emitted.fetch_add(1, Ordering::Relaxed) + 1;
                mux.emit(history_changed(revision));
                std::thread::yield_now();
            }
        })
    };
    let first = relay.next_line(WAIT).expect("an event before the close");
    assert_eq!(first["stream"], 1, "{first}");
    relay.send_queued(&mux, &json!({"id": 40, "cmd": "close-stream", "stream": 1}));
    relay.open(&mux, 1, "apps");
    // Let the emitter go on well past the close.
    let target = emitted.load(Ordering::Relaxed) + 1_000;
    let deadline = Instant::now() + WAIT;
    while emitted.load(Ordering::Relaxed) < target && Instant::now() < deadline {
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    emitter.join().unwrap();

    let mut after_close = None;
    while let Some(line) = relay.next_line(QUIET) {
        if line["id"] == 40 {
            assert_eq!(line["data"], json!({"stream": 1, "closed": true}), "{line}");
            after_close = Some(Vec::new());
        } else if let Some(after) = after_close.as_mut() {
            after.push(line);
        }
    }
    assert_eq!(after_close, Some(Vec::new()), "lines after the close reply");
    mux.shutdown();
}

/// A second `subscribe` of a stream replaces the first: each event comes
/// once.
#[test]
fn resubscribing_a_stream_replaces_its_subscription() {
    let mux = mux("subscribe-again");
    let mut relay = Line::streamed_relay(&mux);
    relay.open(&mux, 4, "history");
    assert_eq!(relay.send(&mux, &subscribe(4))["ok"], true);
    assert_eq!(relay.send(&mux, &subscribe(4))["ok"], true);
    mux.emit(history_changed(1));
    let line = relay.next_line(WAIT).expect("the event");
    assert_eq!(line["stream"], 4, "{line}");
    assert_eq!(relay.next_line(QUIET), None, "the replaced subscription still heard the event");
    mux.shutdown();
}

/// Page streams never live on the main connection or a one-page relay: a
/// `subscribe` naming a stream is refused there.
#[test]
fn subscribe_with_a_stream_is_refused_off_a_streamed_relay() {
    let mux = mux("subscribe-not-here");
    let mut main = Line::connect(&mux);
    let reply = main.send(&mux, &subscribe(1));
    assert_eq!(refusal_reason(&reply), ("origin.forbidden", "page_stream_not_here"));

    let mut relay = Line::connect(&mux);
    let hello = json!({"id": 1, "cmd": "client-hello", "role": "page_relay", "page": "history"});
    assert_eq!(relay.send(&mux, &hello)["ok"], true);
    let reply = relay.send(&mux, &subscribe(1));
    assert_eq!(refusal_reason(&reply), ("origin.forbidden", "page_stream_not_here"));
    mux.shutdown();
}
