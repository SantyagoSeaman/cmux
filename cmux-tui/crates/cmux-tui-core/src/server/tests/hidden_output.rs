//! `set-size-counts {lease, counts}` hides and shows that view's output
//! (p14-energy-119, `terminalHiddenOutputBatch`).

use super::*;

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
