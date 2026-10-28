//! SHARED_BUFFER frames on a real connection (p05-ipc-115): a request body
//! in a pooled region gets the reply its REQUEST frame gets, a 10 MB body is
//! one frame and one descriptor, a write to the region after the send does
//! not change what the request ran with, and a descriptor or frame without
//! the grant closes the connection with `bad_message`.

use std::os::fd::AsRawFd;

use cmux_ipc::shared_buffer::{
    INLINE_MAX, SharedBufferDescriptor, SharedBufferPool, SharedBufferRefusal,
};
use cmux_ipc::{CallAux, CallFlags, FrameKind};
use serde_json::{Value, json};

use super::tests::{Client, Got, method};
use crate::server::line_connection::{ConnectionBadMessage, ConnectionEnd};

const SHARED_BUFFERS: &str = "shared-buffers-v1";

fn call(request_id: u32, name: &str) -> CallAux {
    let method = method(name);
    CallAux {
        request_id,
        method_id: method.id.raw(),
        priority: method.priority,
        flags: CallFlags::empty(),
    }
}

/// A `ping` whose body is `len` bytes long: its padding is a member the
/// command ignores.
fn padded_ping(len: usize) -> Vec<u8> {
    let head = r#"{"cmd":"ping","id":5,"pad":""#;
    let tail = r#""}"#;
    let mut body = head.as_bytes().to_vec();
    body.resize(len - tail.len(), b'x');
    body.extend_from_slice(tail.as_bytes());
    body
}

impl Client {
    /// Sends `body` through `pool` as request `request_id`.
    fn shared(&mut self, pool: &mut SharedBufferPool, request_id: u32, body: &[u8]) -> u16 {
        let staged = pool.stage(0, call(request_id, "ping"), body).unwrap().expect("a region");
        let passed: Vec<_> = staged.fd.iter().map(AsRawFd::as_raw_fd).collect();
        cmux_ipc::fds::send_all(self.socket.as_raw_fd(), &staged.frame, &passed).unwrap();
        staged.region
    }

    fn shared_reply(&mut self, request_id: u32) -> Value {
        match self.next() {
            Some(Got::Frame(header, reply)) => {
                assert_eq!(header.kind(), FrameKind::Reply);
                assert_eq!(header.call().unwrap().request_id, request_id);
                reply
            }
            other => panic!("expected the reply of shared request {request_id}: {other:?}"),
        }
    }
}

#[test]
fn shared_buffer_frames_answer_as_their_request_frame() {
    let mut client = Client::granted_with("shared-reply", &[SHARED_BUFFERS]);
    let body = padded_ping(INLINE_MAX * 2);
    let line = client.line_reply(&serde_json::from_slice(&body).unwrap());
    let mut pool = SharedBufferPool::default();
    client.shared(&mut pool, 40, &body);
    assert_eq!(client.shared_reply(40), line);
    pool.release(40);
    // The region again, with no second descriptor.
    let staged = pool.stage(0, call(41, "ping"), &body).unwrap().unwrap();
    assert!(staged.fd.is_none());
    client.send(&staged.frame);
    assert_eq!(client.shared_reply(41), line);
    assert_eq!(client.finish(), ConnectionEnd::PeerClosed);
}

#[test]
fn shared_buffer_frames_carry_a_ten_mb_paste_in_one_frame() {
    let mut client = Client::granted_with("shared-paste", &[SHARED_BUFFERS]);
    let mut pool = SharedBufferPool::default();
    client.shared(&mut pool, 1, &padded_ping(10 * 1024 * 1024));
    assert_eq!(client.shared_reply(1)["ok"], json!(true));
    let received = client.mux.ipc_stats().snapshot().received;
    let frames: Vec<_> = received
        .iter()
        .filter(|kind| kind.kind != "line")
        .map(|kind| (kind.kind, kind.messages))
        .collect();
    assert_eq!(frames, [("shared_buffer", 1)], "one frame, no chunks: {received:?}");
    assert_eq!(client.finish(), ConnectionEnd::PeerClosed);
}

#[test]
fn shared_buffer_frames_a_write_after_the_send_changes_nothing() {
    let mut client = Client::granted_with("shared-toctou", &[SHARED_BUFFERS]);
    let mut pool = SharedBufferPool::default();
    let body = padded_ping(INLINE_MAX * 4);
    let region = client.shared(&mut pool, 2, &body);
    let reply = client.shared_reply(2);
    assert_eq!((reply["ok"].clone(), reply["id"].clone()), (json!(true), json!(5)), "{reply}");
    // The region is the client's again: a write leaves nothing the daemon
    // reads, and the connection serves on.
    pool.overwrite(region, &vec![0xFF; body.len()]).unwrap();
    let reply = client.line_reply(&json!({"id": 7, "cmd": "ping"}));
    assert_eq!(reply["id"], json!(7));
    assert_eq!(client.finish(), ConnectionEnd::PeerClosed);
}

#[test]
fn shared_buffer_frames_without_the_grant_close_with_bad_message() {
    let mut client = Client::granted_with("shared-no-grant", &[]);
    let mut pool = SharedBufferPool::default();
    client.shared(&mut pool, 1, &padded_ping(INLINE_MAX + 1));
    while client.next().is_some() {}
    assert_eq!(client.finish(), ConnectionEnd::BadMessage(ConnectionBadMessage::UnexpectedFds));

    // A reuse frame needs no descriptor; without the grant it is unrouted.
    let mut client = Client::granted_with("shared-no-grant-frame", &[]);
    let descriptor =
        SharedBufferDescriptor { call: call(1, "ping"), body_len: 1, region: 0, attached: false };
    client.send(&descriptor.frame(0, 1 << 20).unwrap());
    while client.next().is_some() {}
    assert_eq!(
        client.finish(),
        ConnectionEnd::BadMessage(ConnectionBadMessage::UnroutedKind {
            kind: FrameKind::SharedBuffer
        })
    );
}

#[test]
fn shared_buffer_frames_a_bad_region_closes_with_bad_message() {
    let mut client = Client::granted_with("shared-unknown-region", &[SHARED_BUFFERS]);
    let descriptor =
        SharedBufferDescriptor { call: call(1, "ping"), body_len: 1, region: 2, attached: false };
    client.send(&descriptor.frame(0, 1 << 20).unwrap());
    while client.next().is_some() {}
    assert_eq!(
        client.finish(),
        ConnectionEnd::BadMessage(ConnectionBadMessage::SharedBuffer(
            SharedBufferRefusal::UnknownRegion { region: 2 }
        ))
    );

    // A regular file can shrink under the daemon's mapping.
    let mut client = Client::granted_with("shared-file", &[SHARED_BUFFERS]);
    let file = std::fs::File::open("/dev/null").unwrap();
    let descriptor = SharedBufferDescriptor { attached: true, ..descriptor };
    let frame = descriptor.frame(0, 1 << 20).unwrap();
    cmux_ipc::fds::send_all(client.socket.as_raw_fd(), &frame, &[file.as_raw_fd()]).unwrap();
    while client.next().is_some() {}
    assert_eq!(
        client.finish(),
        ConnectionEnd::BadMessage(ConnectionBadMessage::SharedBuffer(
            SharedBufferRefusal::NotSealed { region: 2 }
        ))
    );
}
