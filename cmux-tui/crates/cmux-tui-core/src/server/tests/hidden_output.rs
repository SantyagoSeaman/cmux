//! `set-size-counts {lease, counts}` hides and shows that view's output
//! (p14-energy-119, `terminalHiddenOutputBatch`).

use super::*;
use crate::Surface;
use crate::server::attach_lifecycle::attach_pump::tests::{decoded, next_item};
use crate::server::attach_streams::ATTACH_STREAMS_CAPABILITY;

struct Leased {
    mux: Arc<Mux>,
    surface: SurfaceId,
    client: u64,
    writer: MessageWriter,
    lease: String,
}

impl Leased {
    fn run(&self, request: Value) -> Value {
        handle_command(&self.mux, self.client, json_command(request), &self.writer).unwrap()
    }

    fn output(&self) -> Option<crate::surface::HiddenOutput> {
        self.mux.control_clients.hidden_output(self.client, self.surface, &self.lease)
    }

    fn counts(&self, counts: Option<bool>) {
        let reply = self.run(json!({"cmd": "set-size-counts", "surface": self.surface,
                                    "lease": self.lease, "counts": counts}));
        assert_eq!(reply["outcome"], "applied", "{reply}");
    }
}

/// A view of a new terminal on a connection with leases and shared sizing,
/// attached by bytes at a size.
fn leased(feature: bool) -> Leased {
    let mux = test_mux();
    mux.control_clients.hidden_output.set_for_test(feature);
    let surface = mux.new_workspace(None, Some((80, 24))).unwrap().id;
    let writer = test_writer();
    let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());
    let info = json!({"cmd": "set-client-info", "kind": "frontend",
                      "capabilities": [VIEW_ATTACHMENT_LEASE_CAPABILITY, SHARED_SIZING_CAPABILITY]});
    handle_command(&mux, client, json_command(info), &writer).unwrap();
    let attach = json!({"cmd": "attach-surface", "surface": surface, "cols": 80, "rows": 24});
    let reply = handle_command(&mux, client, json_command(attach), &writer).unwrap();
    let lease = reply["lease"].as_str().expect("a leased attach").to_owned();
    Leased { mux, surface, client, writer, lease }
}

#[test]
fn terminal_stream_set_size_counts_hides_and_shows_the_leased_views_output() {
    let view = leased(true);
    let output = view.output().expect("the attach registered its queue");
    assert!(!output.is_hidden());
    view.counts(Some(false));
    assert!(output.is_hidden(), "counts:false hid the view");
    view.counts(None);
    assert!(!output.is_hidden(), "counts:null showed it");
    view.counts(Some(true));
    assert!(!output.is_hidden(), "only counts:false hides");
}

#[test]
fn terminal_stream_a_participant_selector_never_hides_a_view() {
    let view = leased(true);
    let output = view.output().expect("the attach registered its queue");
    let participant =
        view.mux.terminal_view_participant_id(view.surface, view.client).expect("a participant");
    view.run(json!({"cmd": "set-size-counts", "surface": view.surface,
                    "participant": participant, "counts": false}));
    assert!(!output.is_hidden(), "another device's menu changes sizing alone");
}

#[test]
fn terminal_stream_hidden_output_is_off_without_the_feature() {
    let view = leased(false);
    assert!(view.output().is_none());
    view.counts(Some(false));
}

#[test]
fn terminal_stream_a_detached_view_leaves_no_hidden_output_entry() {
    let view = leased(true);
    assert!(view.output().is_some());
    view.run(json!({"cmd": "detach-attached-view", "surface": view.surface,
                    "lease": view.lease}));
    let deadline = Instant::now() + Duration::from_secs(10);
    while view.output().is_some() {
        assert!(Instant::now() < deadline, "the detach kept the entry");
        std::thread::yield_now();
    }
}

/// The batch interval outside Energy Saver (attach_tap/hidden_batch.rs).
const INTERVAL: Duration = Duration::from_secs(1);

/// A leased view on an attach stream (attach-streams-v1), whose drain runs
/// on the sequenced pool and is woken only by its sources' ready hooks: a
/// byte view, or a snapshot view when `snapshot`.
struct PooledView {
    mux: Arc<Mux>,
    surface: Arc<Surface>,
    client: u64,
    writer: MessageWriter,
    outbound: Arc<BoundedOutbound>,
    lease: String,
}

impl PooledView {
    fn new(name: &str, snapshot: bool) -> Self {
        let mux = Mux::new_for_test(name, SurfaceOptions::default());
        mux.control_clients.hidden_output.set_for_test(true);
        let surface = mux.new_workspace(None, Some((80, 24))).unwrap();
        let (writer, outbound) = captured_writer();
        let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());
        let info = json!({"cmd": "set-client-info", "kind": "frontend",
                          "capabilities": [VIEW_ATTACHMENT_LEASE_CAPABILITY,
                                           SHARED_SIZING_CAPABILITY, ATTACH_STREAMS_CAPABILITY]});
        handle_command(&mux, client, json_command(info), &writer).unwrap();
        let mut attach = json!({"cmd": "attach-surface", "surface": surface.id, "stream": 7,
                                "cols": 80, "rows": 24});
        if snapshot {
            attach["snapshot"] = json!("ghostsnp");
            attach["snapshot_version"] = json!(ghostty_vt::snapshot_version());
        }
        let reply = handle_command(&mux, client, json_command(attach), &writer).unwrap();
        let lease = reply["lease"].as_str().expect("a leased attach").to_owned();
        let view = Self { mux, surface, client, writer, outbound, lease };
        // The READY and whatever the attach queued go out before the view
        // hides: only output written while hidden is held.
        while view.next_item(Duration::from_millis(300)).is_some() {}
        view
    }

    fn next_item(&self, timeout: Duration) -> Option<Value> {
        next_item(&self.outbound, timeout)
    }

    fn hide(&self) {
        let request = json!({"cmd": "set-size-counts", "surface": self.surface.id,
                             "lease": self.lease, "counts": false});
        let reply =
            handle_command(&self.mux, self.client, json_command(request), &self.writer).unwrap();
        assert_eq!(reply["outcome"], "applied", "{reply}");
    }

    /// The next output, and when it arrived. The hide's own `size-state`
    /// is the only other item.
    fn next_output(&self, timeout: Duration) -> (Vec<u8>, Instant) {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let item = self.next_item(left).expect("the held output is sent");
            if item["event"] != "size-state" {
                assert_eq!(item["event"], "output", "nothing else was queued: {item}");
                return (decoded(&item), Instant::now());
            }
        }
    }
}

impl Drop for PooledView {
    fn drop(&mut self) {
        disconnect_client(&self.mux, self.client, false);
        self.mux.shutdown();
    }
}

/// A hidden view that prints once and goes quiet gets its output when the
/// batch is due, from its drain's own timer: no later output, show or
/// exit wakes it.
fn quiet_hidden_view_gets_its_output_when_due(name: &str, snapshot: bool) {
    let view = PooledView::new(name, snapshot);
    view.hide();
    let written = Instant::now();
    view.surface.inject_output_for_test(b"build finished\r\n");
    let (output, arrived) = view.next_output(INTERVAL + Duration::from_secs(3));
    assert_eq!(output, b"build finished\r\n");
    assert!(arrived >= written + INTERVAL, "sent before the batch was due");
}

#[test]
fn terminal_stream_quiet_hidden_byte_view_on_the_pool_gets_its_output_when_due() {
    quiet_hidden_view_gets_its_output_when_due("hidden-pool-bytes", false);
}

#[test]
fn terminal_stream_quiet_hidden_snapshot_view_on_the_pool_gets_its_output_when_due() {
    quiet_hidden_view_gets_its_output_when_due("hidden-pool-snapshot", true);
}

#[test]
fn terminal_stream_hidden_view_on_the_pool_gets_fifty_writes_in_one_second_as_one_frame() {
    let view = PooledView::new("hidden-pool-fifty", false);
    view.hide();
    let written = Instant::now();
    let mut sent = Vec::new();
    for index in 0..50 {
        let line = format!("line-{index}\r\n");
        view.surface.inject_output_for_test(line.as_bytes());
        sent.extend_from_slice(line.as_bytes());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(written.elapsed() < INTERVAL, "the writes spanned one batch");
    let (output, arrived) = view.next_output(INTERVAL + Duration::from_secs(3));
    assert_eq!(output, sent, "one frame");
    assert!(arrived >= written + INTERVAL, "sent before the batch was due");
    let after = view.next_item(Duration::from_millis(300));
    assert!(after.is_none(), "one frame only: {after:?}");
}
