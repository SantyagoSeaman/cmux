//! The schema-generated lanes against the hand-written table they replaced
//! (`old_client_lane`, `old_server_lane`, frozen here from mux_lanes.rs before
//! task 152), name by name, and frames routed from the header alone.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use bytes::{Bytes, BytesMut};
use cmux_ipc::{Aux, CallAux, CallFlags, Frame, MethodTables, RouteRule};

use super::*;

/// The command table before task 152.
fn old_client_lane(name: &str, op: Option<&str>, mode: Option<&str>) -> Lane {
    match name {
        "paste-image" if op == Some("commit") => Lane::Interactive,
        "attach-surface" | "read-screen" | "read-scrollback" | "vt-state" | "paste-image" => {
            Lane::Bulk
        }
        "copy" if mode == Some("scrollback") => Lane::Bulk,
        name if name.starts_with("loopback-") => Lane::Bulk,
        name if name.starts_with("agent-session-") => Lane::Bulk,
        name if name.starts_with("agent-transfer-") => Lane::Bulk,
        "identify" | "ping" | "list-clients" | "list-workspaces" | "export-layout" | "wait-for"
        | "ids" | "list-agents" | "pane-neighbor" | "process-info" | "terminal-ports"
        | "terminal-resources" | "subscribe" | "resolve-terminal" => Lane::Control,
        _ => Lane::Interactive,
    }
}

/// The event table before task 152.
fn old_server_lane(name: &str, scope: Option<&str>) -> Lane {
    match name {
        "output" | "vt-state" | "render-state" | "render-delta" | "frame" | "browser-state"
        | "resized" | "colors-changed" | "scroll-changed" | "detached" => Lane::Bulk,
        name if name.starts_with("loopback-") => Lane::Bulk,
        name if name.starts_with("agent-session-") => Lane::Bulk,
        "overflow" if scope == Some("surface") => Lane::Bulk,
        _ => Lane::Control,
    }
}

const READ_BEHIND_INPUT: &str = "read-only reply that is large (schema priority bulk) or idle \
    (background): it moves to bulk, where it no longer holds keystroke replies behind it; it \
    mutates nothing, so it has nothing to stay ordered behind";
const PASSING_READ: &str = "read-only, and the schema lets it pass the sequence (ordering \
    independent or crosses_stream_ok): it moves to control, as the old table's lookups did";
const SUBSCRIPTION: &str = "opens a subscription: it moves to control with the subscription's \
    events, as subscribe already did";
const STREAM_END: &str = "only attach streams carry it: it ends the stream after the stream's \
    output on bulk, like detached; on control it could overtake that output";

/// Every name whose lane changed, the old and new lane, and why.
const EXPECTED_DIFFERENCES: &[(&str, Lane, Lane, &str)] = &[
    ("cmd terminal-events", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd terminal-resources", Lane::Control, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd machine-listening-tcp", Lane::Interactive, Lane::Control, PASSING_READ),
    ("cmd machine-usage", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd server-stats", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd debug-daemon-locks", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd server-threads", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd server-blocking", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd url-open-subscribe", Lane::Interactive, Lane::Control, SUBSCRIPTION),
    ("cmd terminal-clipboard-subscribe", Lane::Interactive, Lane::Control, SUBSCRIPTION),
    ("cmd get-frontend-browser-history", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd conversation-history", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd terminal-history", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd terminal-read-range", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd cloud-conversation-history", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd subscribe-activity", Lane::Interactive, Lane::Control, SUBSCRIPTION),
    ("cmd conversation-attachment-read", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd cloud-attachment-read", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd list-terminal-commands", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd get-blob", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd feed-local-list", Lane::Interactive, Lane::Control, PASSING_READ),
    ("cmd agent-inventory-refresh", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("cmd fs.read", Lane::Interactive, Lane::Bulk, READ_BEHIND_INPUT),
    ("event stream-end", Lane::Control, Lane::Bulk, STREAM_END),
];

/// The values of a route's refining field to try: absent, and the one value
/// the route refines on.
fn refine_values(route: &cmux_ipc::RemoteRoute) -> Vec<Option<&'static str>> {
    let mut values = vec![None];
    values.extend(route.refine.map(|refine| Some(refine.equals)));
    values
}

#[test]
fn mux_lanes_generated_table_matches_the_old_table_except_listed_reasons() {
    let lanes = RemoteLanes::schema();
    let mut differences = Vec::new();
    for (name, route) in lanes.commands() {
        for value in refine_values(route) {
            let field = route.refine.map(|refine| refine.field);
            let op = value.filter(|_| field == Some(RefineField::Op));
            let mode = value.filter(|_| field == Some(RefineField::Mode));
            let old = old_client_lane(name, op, mode);
            let new = lane_of(route.lane_for(value));
            if old != new {
                differences.push((format!("cmd {name}"), old, new));
            }
        }
    }
    for (name, route) in lanes.events() {
        for value in refine_values(route) {
            let old = old_server_lane(name, value);
            let new = lane_of(route.lane_for(value));
            if old != new {
                differences.push((format!("event {name}"), old, new));
            }
        }
    }
    differences.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected: Vec<(String, Lane, Lane)> = EXPECTED_DIFFERENCES
        .iter()
        .map(|(name, old, new, reason)| {
            assert!(!reason.is_empty(), "{name} has no reason");
            ((*name).to_owned(), *old, *new)
        })
        .collect();
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        differences, expected,
        "differences from the old table must be listed with a reason"
    );
}

/// Subscription events that leave control, and why.
const SUBSCRIPTION_EVENTS_OFF_CONTROL: &[(&str, &str)] = &[(
    "scroll-changed",
    "attach streams carry it too and its line has no field that tells the copies apart: it \
     stays on bulk with the attach output, as before task 152",
)];

#[test]
fn mux_lanes_every_subscription_event_rides_the_subscriptions_lane() {
    // The schema keeps one subscription's events in enqueue order, and an
    // overflow ends the stream: a second lane would let an exit or an
    // overflow pass output enqueued before it.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/sdk-schema.json");
    let text = std::fs::read_to_string(path).unwrap();
    let schema: serde_json::Value = serde_json::from_str(&text).unwrap();
    let lanes = RemoteLanes::schema();
    let mut subscriptions = 0;
    for (command, node) in schema["commands"].as_object().unwrap() {
        let stream = &node["stream"];
        if stream["kind"] != "subscribe" {
            continue;
        }
        subscriptions += 1;
        let route = lanes.command(command).unwrap();
        assert_eq!(lane_of(route.lane_for(None)), Lane::Control, "cmd {command}");
        for event in stream["event_names"].as_array().unwrap() {
            let event = event.as_str().unwrap();
            if SUBSCRIPTION_EVENTS_OFF_CONTROL.iter().any(|(name, _)| *name == event) {
                continue;
            }
            let route = lanes.event(event).unwrap_or_else(|| panic!("{command}: {event}"));
            // A subscription's line carries no attach `scope`.
            assert_eq!(lane_of(route.lane_for(None)), Lane::Control, "{command}: {event}");
            assert_eq!(lane_of(route.frame_lane()), Lane::Control, "{command}: {event} frame");
        }
    }
    assert!(subscriptions >= 4, "the schema's subscribe commands were not found");
    let tracker = MuxLaneTracker::default();
    for line in [
        &br#"{"event":"surface-output","surface":1}"#[..],
        br#"{"event":"surface-exited","surface":1}"#,
        br#"{"event":"overflow","error":"subscriber fell behind"}"#,
        br#"{"event":"machine-usage-changed"}"#,
        br#"{"event":"activity-changed"}"#,
    ] {
        assert_eq!(tracker.classify_server_line(line), Some(Lane::Control));
    }
}

#[test]
fn mux_lanes_every_schema_name_has_a_route() {
    let tables = MethodTables::schema();
    let lanes = RemoteLanes::schema();
    for method in tables.v12_families().flat_map(|family| family.iter()) {
        assert!(lanes.command(method.name).is_some(), "{}", method.name);
    }
    for event in tables.event_families().flat_map(|family| family.iter()) {
        assert!(lanes.event(event.name).is_some(), "{}", event.name);
    }
    // The events the daemon emits on agent-session-attach-v1 streams, which
    // the schema does not declare yet, are routed as unlisted.
    for name in ["agent-session-record", "agent-session-permission", "agent-session-closed"] {
        assert_eq!(lanes.event(name).map(|route| route.rule), Some(RouteRule::Unlisted), "{name}");
    }
}

#[test]
fn loopback_forward_lines_use_the_bulk_lane_in_both_directions() {
    for line in [
        &br#"{"id":1,"cmd":"loopback-open","stream":1,"host":"localhost","port":3000}"#[..],
        br#"{"cmd":"loopback-data","stream":1,"data":"eA=="}"#,
        br#"{"cmd":"loopback-credit","stream":1,"bytes":65536}"#,
        br#"{"cmd":"loopback-shutdown","stream":1}"#,
        br#"{"cmd":"loopback-close","stream":1}"#,
    ] {
        assert_eq!(classify_client_line(line), Lane::Bulk);
    }
    let tracker = MuxLaneTracker::default();
    for line in [
        &br#"{"event":"loopback-data","stream":1,"data":"eA=="}"#[..],
        br#"{"event":"loopback-credit","stream":1,"bytes":65536}"#,
        br#"{"event":"loopback-eof","stream":1}"#,
        br#"{"event":"loopback-closed","stream":1}"#,
    ] {
        assert_eq!(tracker.classify_server_line(line), Some(Lane::Bulk));
    }
    tracker.observe_request(br#"{"id":7,"cmd":"loopback-open"}"#, Lane::Bulk);
    assert_eq!(tracker.classify_server_line(br#"{"id":7,"ok":true}"#), Some(Lane::Bulk));
}

#[test]
fn agent_transfer_pages_use_the_bulk_lane() {
    for line in [
        &br#"{"id":1,"cmd":"agent-transfer-write","receive_id":"r","path":"a","offset":0,"data":""}"#[..],
        br#"{"id":2,"cmd":"agent-transfer-read","transfer_id":"t","path":"a","offset":0}"#,
    ] {
        assert_eq!(classify_client_line(line), Lane::Bulk);
    }
}

#[test]
fn agent_session_attach_lines_use_the_bulk_lane_in_both_directions() {
    for line in [
        &br#"{"id":1,"cmd":"agent-session-attach","surface":3}"#[..],
        br#"{"id":2,"cmd":"agent-session-events","surface":3,"after_seq":9}"#,
        br#"{"id":3,"cmd":"agent-session-prompt","surface":3,"prompt_id":"p","text":"hi"}"#,
        br#"{"id":4,"cmd":"agent-session-detach","surface":3}"#,
    ] {
        assert_eq!(classify_client_line(line), Lane::Bulk);
    }
    let tracker = MuxLaneTracker::default();
    for line in [
        &br#"{"event":"agent-session-record","surface":3,"record":{"seq":2}}"#[..],
        br#"{"event":"agent-session-permission","surface":3,"request":{}}"#,
        br#"{"event":"agent-session-closed","surface":3,"reason":"lagged"}"#,
    ] {
        assert_eq!(tracker.classify_server_line(line), Some(Lane::Bulk));
    }
    tracker.observe_request(br#"{"id":1,"cmd":"agent-session-attach"}"#, Lane::Bulk);
    assert_eq!(tracker.classify_server_line(br#"{"id":1,"ok":true}"#), Some(Lane::Bulk));
}

#[test]
fn keystrokes_and_terminal_output_use_distinct_lanes() {
    let tracker = MuxLaneTracker::default();
    let request = br#"{"id":7,"cmd":"send","surface":1,"bytes":"YQ=="}"#;
    let lane = classify_client_line(request);
    assert_eq!(lane, Lane::Interactive);
    tracker.observe_request(request, lane);
    assert_eq!(tracker.classify_server_line(br#"{"id":7,"ok":true}"#), Some(Lane::Interactive));
    assert_eq!(
        tracker.classify_server_line(br#"{"event":"output","surface":1,"data":"Yg=="}"#),
        Some(Lane::Bulk)
    );
}

#[test]
fn refined_lines_pick_their_lane_from_the_envelope_field() {
    assert_eq!(
        classify_client_line(br#"{"id":1,"cmd":"paste-image","op":"commit","surface":1}"#),
        Lane::Interactive
    );
    assert_eq!(
        classify_client_line(br#"{"id":1,"cmd":"paste-image","op":"chunk","surface":1}"#),
        Lane::Bulk
    );
    assert_eq!(
        classify_client_line(br#"{"id":1,"cmd":"copy","mode":"scrollback","surface":1}"#),
        Lane::Bulk
    );
    assert_eq!(
        classify_client_line(br#"{"id":1,"cmd":"copy","mode":"screen","surface":1}"#),
        Lane::Interactive
    );
    let tracker = MuxLaneTracker::default();
    assert_eq!(
        tracker.classify_server_line(br#"{"event":"overflow","scope":"surface","surface":1}"#),
        Some(Lane::Bulk)
    );
    assert_eq!(tracker.classify_server_line(br#"{"event":"overflow"}"#), Some(Lane::Control));
}

#[test]
fn unknown_and_malformed_lines_keep_their_old_lanes() {
    assert_eq!(classify_client_line(br#"{"id":1,"cmd":"not-a-command"}"#), Lane::Interactive);
    assert_eq!(classify_client_line(br#"{"id":1}"#), Lane::Control);
    assert_eq!(classify_client_line(b"not json"), Lane::Control);
    let tracker = MuxLaneTracker::default();
    assert_eq!(tracker.classify_server_line(br#"{"event":"not-an-event"}"#), Some(Lane::Control));
    assert_eq!(tracker.classify_server_line(b"not json"), Some(Lane::Control));
    assert_eq!(tracker.classify_server_line(br#"{"id":99,"ok":true}"#), Some(Lane::Control));
}

struct CountingAllocator;

std::thread_local! {
    static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

// Rust installs this allocator for the whole cmux-remote unit-test binary;
// thread-local counting keeps unrelated test threads out of each sample.
#[global_allocator]
static TEST_ALLOCATOR: CountingAllocator = CountingAllocator;

fn count_allocation() {
    COUNT_ALLOCATIONS
        .try_with(|enabled| {
            if enabled.get() {
                ALLOCATION_COUNT.with(|count| count.set(count.get().saturating_add(1)));
            }
        })
        .ok();
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_allocation();
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

/// Allocations `work` makes on this thread.
fn allocations(work: impl FnOnce()) -> usize {
    ALLOCATION_COUNT.with(|count| count.set(0));
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
    work();
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
    ALLOCATION_COUNT.with(Cell::get)
}

fn call_frame(kind: FrameKind, method_id: u16, payload: &'static [u8]) -> Vec<u8> {
    let aux = Aux::Call(CallAux {
        request_id: 7,
        method_id,
        priority: PriorityHint::Control,
        flags: CallFlags::empty(),
    });
    let frame = Frame::new(kind, 0, aux, Bytes::from_static(payload)).unwrap();
    let mut out = BytesMut::new();
    frame.encode_into(&mut out);
    out.to_vec()
}

fn method_id(name: &str) -> u16 {
    MethodTables::schema().v12_id(name).unwrap().raw()
}

fn event_id(name: &str) -> u16 {
    MethodTables::schema().event_id(name).unwrap().raw()
}

#[test]
fn mux_lanes_frames_route_from_the_header_with_no_json_parse() {
    // Payloads that are not JSON: a parse would fail and give Control.
    let garbage: &'static [u8] = b"\xff not json \n {";
    let send = call_frame(FrameKind::Request, method_id("send"), garbage);
    let history = call_frame(FrameKind::Request, method_id("terminal-history"), garbage);
    let history_reply = call_frame(FrameKind::Reply, method_id("terminal-history"), garbage);
    let ping = call_frame(FrameKind::Request, method_id("ping"), garbage);
    let paste = call_frame(FrameKind::Request, method_id("paste-image"), garbage);
    let output = call_frame(FrameKind::TypedEvent, event_id("output"), garbage);
    let unknown = call_frame(FrameKind::Request, 0x5F_FF, garbage);
    let stream_output = {
        let frame =
            Frame::new(FrameKind::Output, 3, Aux::Offset(9), Bytes::from_static(garbage)).unwrap();
        let mut out = BytesMut::new();
        frame.encode_into(&mut out);
        out.to_vec()
    };
    let tracker = MuxLaneTracker::default();
    let mut lanes = Vec::with_capacity(8);
    let count = allocations(|| {
        for message in [&send, &history, &ping, &paste, &unknown, &stream_output] {
            lanes.push(classify_client_line(message));
        }
        tracker.observe_request(&history, Lane::Bulk);
        for message in [&history_reply, &output] {
            lanes.push(tracker.classify_server_line(message).unwrap());
        }
    });
    assert_eq!(count, 0, "a frame is routed from its header: no parse, no allocation");
    assert_eq!(
        lanes,
        [
            Lane::Interactive,
            Lane::Bulk,
            Lane::Control,
            // A framed paste cannot be told from its commit: it stays with input.
            Lane::Interactive,
            Lane::Interactive,
            Lane::Bulk,
            Lane::Bulk,
            Lane::Bulk,
        ]
    );
    // Frames are not tracked: the reply names its method.
    assert!(tracker.state.lock().unwrap().requests.is_empty());
}

#[test]
fn mux_lanes_bad_frame_headers_ride_control() {
    assert_eq!(classify_client_line(&[MAGIC, 0xEE]), Lane::Control);
    let mut header = [0_u8; HEADER_LEN];
    header[0] = MAGIC;
    header[1] = 0xEE;
    assert_eq!(classify_client_line(&header), Lane::Control);
    let tracker = MuxLaneTracker::default();
    assert_eq!(tracker.classify_server_line(&header), Some(Lane::Control));
}

/// Every message `read_bounded_line_with_limit` reads from `stream`, through a
/// small buffer so a frame header spans several fills.
async fn read_messages(stream: &[u8], maximum: usize) -> Vec<Vec<u8>> {
    let mut reader = tokio::io::BufReader::with_capacity(5, stream);
    let mut messages = Vec::new();
    let mut line = Vec::new();
    while crate::mux_codec::read_bounded_line_with_limit(&mut reader, &mut line, maximum)
        .await
        .unwrap()
        > 0
    {
        messages.push(line.clone());
    }
    messages
}

#[tokio::test]
async fn mux_lanes_frames_with_newlines_are_read_and_routed_whole() {
    // 0x0A in the payloads: a newline reader would cut each frame in two.
    let send = call_frame(FrameKind::Request, method_id("send"), b"a\nb");
    let history = call_frame(FrameKind::Request, method_id("terminal-history"), b"\n\n");
    let output = call_frame(FrameKind::TypedEvent, event_id("output"), b"x\ny");
    let line = br#"{"id":1,"cmd":"ping"}
"#
    .to_vec();
    let mut bad = vec![0_u8; HEADER_LEN];
    bad[0] = MAGIC;
    bad[1] = 0xEE;
    bad[2] = b'\n';
    let client = [send.as_slice(), &line, &history, &bad, &line].concat();
    let messages = read_messages(&client, crate::mux_codec::MAX_MUX_UPLOAD_LINE_BYTES).await;
    // A bad header ends its message after the header; it rides control.
    assert_eq!(messages, [send.clone(), line.clone(), history, bad, line.clone()]);
    let lanes: Vec<Lane> = messages.iter().map(|message| classify_client_line(message)).collect();
    assert_eq!(lanes, [Lane::Interactive, Lane::Control, Lane::Bulk, Lane::Control, Lane::Control]);

    let server = [output.as_slice(), &line].concat();
    let messages = read_messages(&server, crate::mux_codec::MAX_MUX_DOWNLOAD_LINE_BYTES).await;
    assert_eq!(messages, [output, line]);
    let tracker = MuxLaneTracker::default();
    assert_eq!(tracker.classify_server_line(&messages[0]), Some(Lane::Bulk));

    // A frame over the limit reads limit + 1 bytes, so the caller refuses it
    // as it refuses a long line.
    let messages = read_messages(&send, send.len() - 2).await;
    assert_eq!(messages[0].len(), send.len() - 1);
}
