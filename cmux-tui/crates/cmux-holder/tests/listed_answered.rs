//! p01-process-210 review: LISTED names the highest offset a daemon
//! acknowledged (`Listed::answered`), not the ring end. A restarted daemon
//! drops the query answers of a replay only up to there: output no daemon
//! parsed (printed while none ran) is answered.

#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use cmux_holder::SpawnRequest;
use cmux_holder::client::{Event, HolderClient, launch};

mod support;

const TOKEN: [u8; 32] = [0x5a; 32];

/// A PING and its PONG: the holder handled everything sent before it.
fn holder_turn(client: &mut HolderClient, nonce: u64) {
    client.ping(nonce).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match client.next_event(deadline).unwrap() {
            Event::Pong { nonce: answered } if answered == nonce => return,
            Event::Pong { .. } | Event::Data { .. } | Event::Gap { .. } => {}
            other => panic!("unexpected {other:?} before PONG {nonce}"),
        }
    }
}

#[test]
fn listed_names_the_highest_offset_a_daemon_acknowledged() {
    let dir = support::unique_dir("cmla");
    let socket = dir.join("h.sock");
    let mut holder = launch(Path::new(env!("CARGO_BIN_EXE_cmux-holder")), &socket, &TOKEN).unwrap();
    let (mut client, _) = HolderClient::connect(&socket, &TOKEN).unwrap();
    let mut id = [0u8; 16];
    id[0] = 1;
    let request = SpawnRequest {
        id,
        cols: 80,
        rows: 24,
        px_w: 0,
        px_h: 0,
        cwd: b"/tmp".to_vec(),
        argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"printf ready; exec cat".to_vec()],
        env: vec![b"PATH=/usr/bin:/bin".to_vec()],
    };
    client.spawn(1, &request).unwrap();
    client.attach(1, 0).unwrap();
    let mut received = 0u64;
    let deadline = Instant::now() + Duration::from_secs(30);
    while received < 5 {
        if let Event::Data { bytes, .. } = client.next_event(deadline).unwrap() {
            received += bytes.len() as u64;
        }
    }
    client.ack(1, 3).unwrap();
    holder_turn(&mut client, 1);
    client.end();

    let (mut client, listed) = HolderClient::connect(&socket, &TOKEN).unwrap();
    let end = listed[0].end;
    assert!(end >= 5, "{listed:?}");
    assert_eq!(listed[0].answered, 3, "the ACK, not the ring end");
    // An ATTACH resets the credit to what this daemon parsed, and a lower
    // ACK follows: what an earlier daemon answered stays answered.
    client.attach_parsed(1, end, 0).unwrap();
    client.ack(1, 2).unwrap();
    holder_turn(&mut client, 2);
    client.end();

    let (mut client, listed) = HolderClient::connect(&socket, &TOKEN).unwrap();
    assert_eq!(listed[0].answered, 3, "never lowered");
    // No daemon parsed past what the holder read.
    client.attach(1, end).unwrap();
    client.ack(1, end + 1000).unwrap();
    holder_turn(&mut client, 3);
    client.end();
    let (client, listed) = HolderClient::connect(&socket, &TOKEN).unwrap();
    assert_eq!(listed[0].answered, listed[0].end, "capped at the ring end");
    client.end();

    let _ = holder.kill();
    let _ = holder.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
