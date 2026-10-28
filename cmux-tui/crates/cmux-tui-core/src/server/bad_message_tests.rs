//! Bad messages and the crash-loop breaker (p05-ipc-191), over a real
//! socket pair. Each of 20 malformed inputs closes its connection with its
//! reason and `ipc.bad_message` + 1, and nothing panics. Three framed
//! connections closed as bad messages within 60 s turn frames off for the
//! daemon's life: `identify` stops offering them and `set-client-info` stops
//! granting them, a connection that holds the grant keeps using it, and
//! lines go on. A peer that closes inside a frame is not counted.

use std::io::Read;

use super::*;
use crate::server::line_connection::IPC_FRAMES_CAPABILITY;

/// A REQUEST, REPLY or CANCEL frame's bytes with any aux bytes, valid or not.
fn raw_frame(kind: u8, stream: u16, len: u32, aux: [u8; 8], payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0xFF, kind];
    bytes.extend_from_slice(&stream.to_le_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&aux);
    bytes.extend_from_slice(payload);
    bytes
}

/// A call aux: request id 1, `method`, `priority` and `flags` bytes.
fn call_aux(method: u16, priority: u8, flags: u8) -> [u8; 8] {
    let [m0, m1] = method.to_le_bytes();
    [1, 0, 0, 0, m0, m1, priority, flags]
}

fn ping_method() -> &'static cmux_ipc::MethodAttrs {
    let tables = cmux_ipc::MethodTables::schema();
    tables.v12_id("ping").and_then(|id| tables.method(id)).expect("ping is a v12 method")
}

/// A call frame of `kind` for `ping`'s method id with `priority`.
fn ping_call(kind: FrameKind, priority: PriorityHint, body: &[u8]) -> Vec<u8> {
    let aux = Aux::Call(CallAux {
        request_id: 1,
        method_id: ping_method().id.raw(),
        priority,
        flags: CallFlags::empty(),
    });
    let header = FrameHeader::new(kind, 0, u32::try_from(body.len()).unwrap(), aux).unwrap();
    let mut bytes = header.encode().to_vec();
    bytes.extend_from_slice(body);
    bytes
}

/// How a fixture reaches the daemon.
enum Send {
    /// Bytes on a connection granted frames.
    Framed(Vec<u8>),
    /// Bytes on a connection that never declared frames.
    Lines(Vec<u8>),
    /// A line with a descriptor beside it.
    Descriptor,
}

/// The 20 malformed inputs and the reason each closes with.
fn fixtures() -> Vec<(&'static str, Send)> {
    let control = PriorityHint::Control.wire();
    let ping = ping_method();
    let body = br#"{"cmd":"ping"}"#;
    vec![
        ("unknown_kind", Send::Framed(raw_frame(0, 0, 0, [0; 8], b""))),
        ("unknown_kind", Send::Framed(raw_frame(12, 1, 0, [0; 8], b""))),
        (
            "oversize",
            Send::Framed(raw_frame(
                16,
                0,
                (32 << 20) + 1,
                call_aux(ping.id.raw(), control, 0),
                b"",
            )),
        ),
        ("unknown_priority", Send::Framed(raw_frame(16, 0, 0, call_aux(ping.id.raw(), 4, 0), b""))),
        (
            "unknown_flags",
            Send::Framed(raw_frame(16, 0, 0, call_aux(ping.id.raw(), control, 0x20), b"")),
        ),
        ("data_on_control_stream", Send::Framed(raw_frame(1, 0, 1, [0; 8], b"x"))),
        ("data_on_control_stream", Send::Framed(raw_frame(9, 0, 1, [0; 8], b"x"))),
        ("invalid_utf8", Send::Lines(b"{\"id\":1,\"cmd\":\"\xc3\x28\"}\n".to_vec())),
        ("invalid_utf8", Send::Lines(b"\x80\n".to_vec())),
        // The peer closes inside a header, then inside a payload.
        ("truncated", Send::Framed(raw_frame(16, 0, 0, call_aux(0, 0, 0), b"")[..9].to_vec())),
        (
            "truncated",
            Send::Framed(ping_call(FrameKind::Request, PriorityHint::Control, body)[..20].to_vec()),
        ),
        (
            "frames_not_negotiated",
            Send::Lines(ping_call(FrameKind::Request, PriorityHint::Control, body)),
        ),
        ("unrouted_kind", Send::Framed(ping_call(FrameKind::Reply, PriorityHint::Control, b"{}"))),
        ("unrouted_kind", Send::Framed(raw_frame(1, 3, 1, [0; 8], b"x"))),
        ("unknown_stream", Send::Framed(raw_frame(9, 7, 1, [0; 8], b"x"))),
        ("unknown_method", Send::Framed(raw_frame(16, 0, 2, call_aux(0x01FF, control, 0), b"{}"))),
        (
            "priority_mismatch",
            Send::Framed(ping_call(FrameKind::Request, PriorityHint::Bulk, body)),
        ),
        (
            "method_mismatch",
            Send::Framed(ping_call(
                FrameKind::Request,
                PriorityHint::Control,
                br#"{"cmd":"identify"}"#,
            )),
        ),
        (
            "cancel_payload",
            Send::Framed(ping_call(FrameKind::Cancel, PriorityHint::Control, b"{}")),
        ),
        ("unexpected_fds", Send::Descriptor),
    ]
}

/// Declares frames in a real `set-client-info` at this daemon's revision;
/// the reply's `ok`.
fn declare_frames(peer: &mut Peer, id: u64) -> Value {
    let (current, _) = api_revision_wire::identify_fields();
    let declare = json!({
        "id": id,
        "cmd": "set-client-info",
        "name": "frames",
        "capabilities": ["ipc-frames-v2"],
        "api_revision": current,
    });
    peer.send(format!("{declare}\n").as_bytes());
    peer.reply().unwrap()
}

/// Sends the rest of the peer's side, closes it, and returns how the
/// connection ended. Nothing is answered after the bad message.
fn close_after(mut peer: Peer, send: Send) -> ConnectionEnd {
    match send {
        Send::Framed(bytes) | Send::Lines(bytes) => {
            // The daemon may close before the last bytes; their write may fail.
            let _ = peer.socket.write_all(&bytes);
        }
        Send::Descriptor => {
            let passed = std::fs::File::open("/dev/null").unwrap();
            cmux_ipc::fds::send_all(peer.socket.as_raw_fd(), &ping(2), &[passed.as_raw_fd()])
                .unwrap();
        }
    }
    let _ = peer.socket.shutdown(Shutdown::Write);
    assert_eq!(peer.reply(), None, "a bad message is never answered");
    peer.served.join().expect("the connection's thread did not panic")
}

#[test]
fn bad_message_each_malformed_input_closes_with_its_reason_and_counts_once() {
    let fixtures = fixtures();
    assert_eq!(fixtures.len(), 20);
    for (index, (reason, send)) in fixtures.into_iter().enumerate() {
        let label = format!("bad-{index}-{reason}");
        let mut peer = match send {
            Send::Framed(_) => {
                let mut peer = same_user_peer(&label);
                assert_eq!(declare_frames(&mut peer, 1)["ok"], json!(true), "{label}");
                peer
            }
            Send::Lines(_) | Send::Descriptor => connect(&label, None),
        };
        peer.send(&ping(1));
        assert_eq!(peer.reply().unwrap()["id"], json!(1), "{label}");
        let mux = peer.mux.clone();
        match close_after(peer, send) {
            ConnectionEnd::BadMessage(bad) => assert_eq!(bad.reason(), reason, "{label}: {bad}"),
            end => panic!("{label}: {end:?}"),
        }
        let ipc = mux.ipc_stats().snapshot();
        assert_eq!(ipc.bad_message, 1, "{label}");
        assert_eq!(ipc.streams.open, 0, "{label}");
    }
}

/// A connection on `mux` from a direct same-user peer, after one ping; its
/// client id.
fn peer_on(mux: &Arc<Mux>) -> (Peer, u64) {
    let (socket, server) = UnixStream::pair().unwrap();
    let served_mux = mux.clone();
    let served = std::thread::spawn(move || {
        let stream: Box<dyn transport::Stream> = Box::new(server);
        let peer = origin_gate::AcceptedPeer::resolve(&served_mux, &*stream);
        serve_connection(
            served_mux,
            stream,
            peer,
            Arc::new(RenderService::new()),
            None,
            ClientTransport::Unix,
            &admission::LocalAdmission,
            &mut RequestFrames::default(),
        )
    });
    let read_half = socket.try_clone().unwrap();
    read_half.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut peer = Peer { mux: mux.clone(), socket, replies: BufReader::new(read_half), served };
    peer.send(&ping(99));
    assert_eq!(peer.reply().unwrap()["id"], json!(99));
    let client = {
        let state = mux.control_clients.state.lock().unwrap();
        state
            .clients
            .iter()
            .filter(|(_, record)| matches!(record.transport, ClientTransport::Unix))
            .map(|(client, _)| *client)
            .max()
            .expect("the connection's client")
    };
    origin_gate::set_peer_process_for_test(
        mux,
        client,
        crate::request_origin::PeerProcess::SameUser,
    );
    (peer, client)
}

/// Whether `identify` on `peer` offers frames.
fn offers_frames(peer: &mut Peer, id: u64) -> bool {
    peer.send(format!("{}\n", json!({"id": id, "cmd": "identify"})).as_bytes());
    let reply = peer.reply().unwrap();
    let capabilities = reply["data"]["capabilities"].as_array().cloned().unwrap_or_default();
    capabilities.contains(&json!(IPC_FRAMES_CAPABILITY))
}

fn unknown_method_frame() -> Vec<u8> {
    raw_frame(16, 0, 2, call_aux(0x01FF, PriorityHint::Control.wire(), 0), b"{}")
}

#[test]
fn bad_message_breaker_turns_frames_off_after_the_third_framed_close() {
    let mux = Mux::new_for_test("bad-message-breaker", crate::SurfaceOptions::default());
    mux.control_clients.ipc_transports.bind_all_for_test();

    // Bad lines on connections that never negotiated frames do not count.
    for _ in 0..3 {
        let (peer, _) = peer_on(&mux);
        let end = close_after(peer, Send::Lines(b"\x80\n".to_vec()));
        assert!(matches!(end, ConnectionEnd::BadMessage(_)), "{end:?}");
    }
    assert!(!mux.control_clients.frame_breaker.is_tripped());

    // A connection that holds the grant while the breaker trips.
    let (mut holder, _) = peer_on(&mux);
    assert_eq!(declare_frames(&mut holder, 1)["ok"], json!(true));

    for close in 1..=3 {
        let (mut peer, _) = peer_on(&mux);
        assert!(offers_frames(&mut peer, 1), "close {close}: frames still on");
        assert_eq!(declare_frames(&mut peer, 2)["ok"], json!(true));
        let end = close_after(peer, Send::Framed(unknown_method_frame()));
        assert!(
            matches!(&end, ConnectionEnd::BadMessage(bad) if bad.reason() == "unknown_method"),
            "{end:?}"
        );
        assert_eq!(mux.control_clients.frame_breaker.is_tripped(), close == 3, "close {close}");
    }
    assert_eq!(mux.ipc_stats().snapshot().bad_message, 6);

    // Frames are off for this daemon's life: not offered, not granted.
    let (mut peer, client) = peer_on(&mux);
    assert!(!offers_frames(&mut peer, 1));
    assert_eq!(declare_frames(&mut peer, 2)["ok"], json!(true));
    assert!(!mux.control_clients.supports_capability(client, IPC_FRAMES_CAPABILITY));
    // Lines go on.
    peer.send(&ping(3));
    assert_eq!(peer.reply().unwrap()["id"], json!(3));
    let end = close_after(
        peer,
        Send::Framed(ping_call(FrameKind::Request, PriorityHint::Control, b"{}")),
    );
    assert_eq!(
        end,
        ConnectionEnd::BadMessage(ConnectionBadMessage::Wire(BadMessage::FramesNotNegotiated))
    );

    // The connection that held the grant keeps it: its frame is answered,
    // and its lines still work.
    holder.send(&ping_call(FrameKind::Request, PriorityHint::Control, br#"{"cmd":"ping"}"#));
    let (header, reply) = read_frame(&mut holder);
    assert_eq!(header.kind(), FrameKind::Reply);
    assert_eq!(reply["ok"], json!(true), "{reply}");
    holder.send(&ping(4));
    assert_eq!(holder.reply().unwrap()["id"], json!(4));
    assert_eq!(holder.finish(), ConnectionEnd::PeerClosed);
    let ipc = server_stats::server_stats(&mux, Some(&["ipc".to_owned()])).ipc.unwrap();
    assert_eq!(
        ipc.frame_breaker,
        crate::diagnostics::IpcFrameBreakerSnapshot { tripped: true, framed_bad_message: 3 }
    );
}

/// The next message on `peer`'s socket, which must be a frame; its header
/// and JSON payload.
fn read_frame(peer: &mut Peer) -> (FrameHeader, Value) {
    let mut header = [0; cmux_ipc::HEADER_LEN];
    peer.replies.read_exact(&mut header).unwrap();
    let header = FrameHeader::decode(header, u32::MAX).unwrap();
    let mut payload = vec![0; header.len() as usize];
    peer.replies.read_exact(&mut payload).unwrap();
    (header, serde_json::from_slice(&payload).unwrap())
}

/// A peer that dies, or whose writer gives up, in the middle of a frame is a
/// lost peer: the close still counts as `truncated`, but never toward the
/// breaker, however often it happens.
#[test]
fn bad_message_a_close_inside_a_frame_never_trips_the_breaker() {
    let mux = Mux::new_for_test("bad-message-truncated", crate::SurfaceOptions::default());
    mux.control_clients.ipc_transports.bind_all_for_test();
    let cut = ping_call(FrameKind::Request, PriorityHint::Control, br#"{"cmd":"ping"}"#);
    for close in 1..=frame_breaker::TRIP_CLOSES * 2 {
        let (mut peer, _) = peer_on(&mux);
        assert_eq!(declare_frames(&mut peer, 1)["ok"], json!(true));
        let bytes = if close % 2 == 0 { &cut[..9] } else { &cut[..20] };
        let end = close_after(peer, Send::Framed(bytes.to_vec()));
        assert!(
            matches!(&end, ConnectionEnd::BadMessage(bad) if bad.reason() == "truncated"),
            "{end:?}"
        );
    }
    assert!(!mux.control_clients.frame_breaker.is_tripped());
    let (mut peer, _) = peer_on(&mux);
    assert!(offers_frames(&mut peer, 1), "frames are still offered");
    let ipc = server_stats::server_stats(&mux, Some(&["ipc".to_owned()])).ipc.unwrap();
    assert_eq!(ipc.bad_message, 6);
    assert_eq!(ipc.frame_breaker, crate::diagnostics::IpcFrameBreakerSnapshot::default());
}
