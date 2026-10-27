//! p05-ipc-111: the daemon's terminal output ring. A ring viewer's terminal
//! writes every byte its parser accepted into the ring at the snapshot
//! offsets, and the viewer's queue holds one merged advance however much
//! output arrives.

use super::*;
use cmux_ipc::output_ring::{DEFAULT_CAPACITY, LAYOUT, OutputRingReader};
use std::os::fd::AsFd;

/// A terminal whose program prints nothing, so every output byte is one a
/// test fed.
fn quiet_terminal(name: &str) -> (Arc<Mux>, Arc<Surface>) {
    let mux = Mux::new_for_test(name, SurfaceOptions::default());
    let options = SurfaceOptions {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "read _".into()]),
        ..SurfaceOptions::default()
    };
    let surface = Surface::spawn_for_test(1, options, Arc::downgrade(&mux)).unwrap();
    (mux, surface)
}

/// The reader's path: parse, then broadcast under the terminal lock. The
/// bytes the parser accepted.
fn feed(pty: &PtySurface, bytes: &[u8]) -> Vec<u8> {
    let mut term = pty.term.lock().unwrap();
    let normalized = term.vt_write_with_normalized(bytes).into_owned();
    pty.broadcast_attach_output(&normalized);
    normalized
}

fn line(index: usize) -> Vec<u8> {
    format!("line {index:06} {}\r\n", "abcdefghij".repeat(index % 7 + 1)).into_bytes()
}

fn drain_output(receiver: &AttachFrameReceiver, into: &mut Vec<u8>) {
    while let Ok(frame) = receiver.try_recv() {
        if let Some(output) = frame.output() {
            into.extend_from_slice(output);
        }
    }
}

fn ring_attach(surface: &Surface) -> (snapshot_attach::SnapshotAttachStream, OutputRingGrant) {
    let attach =
        surface.attach_snapshot_ring_stream(AttachLifecycle::default(), 1 << 20, false).unwrap();
    (attach.stream, attach.ring.unwrap())
}

/// The ring holds the bytes a byte viewer of the same terminal received, at
/// the same offsets, after it wrapped twice.
#[test]
fn output_ring_attach_ring_bytes_equal_the_parser_bytes_at_their_offsets() {
    let (_mux, surface) = quiet_terminal("output-ring-bytes");
    let pty = surface.as_pty().unwrap();
    let bytes_viewer = surface.attach_stream().unwrap();
    let (_stream, grant) = ring_attach(&surface);
    assert_eq!((grant.layout, grant.capacity), (LAYOUT, DEFAULT_CAPACITY as u64));
    assert_eq!(grant.end, grant.base, "a new ring starts empty at the snapshot offset");
    assert!(grant.base >= bytes_viewer.offset);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();

    let (mut parsed, mut received) = (Vec::new(), Vec::new());
    let mut index = 0;
    while parsed.len() < 2 * DEFAULT_CAPACITY + DEFAULT_CAPACITY / 2 {
        let chunk: Vec<u8> = (0..200)
            .flat_map(|_| {
                index += 1;
                line(index)
            })
            .collect();
        parsed.extend(feed(pty, &chunk));
        drain_output(&bytes_viewer.stream, &mut received);
    }
    drain_output(&bytes_viewer.stream, &mut received);

    let start = grant.base - bytes_viewer.offset;
    assert_eq!(&received[start as usize..], &parsed[..], "the byte viewer got the parser's bytes");
    let end = ring.end();
    assert_eq!(end, grant.base + parsed.len() as u64, "the ring's end counts every parsed byte");
    assert_eq!(
        surface.snapshot_stream_position().unwrap().1,
        end,
        "the ring end is the snapshot offset"
    );
    let from = end - DEFAULT_CAPACITY as u64;
    let mut in_ring = Vec::new();
    ring.read_into(from, end, &mut in_ring).unwrap();
    let at = (from - grant.base) as usize;
    assert_eq!(in_ring, parsed[at..], "the ring holds the parser's last capacity bytes");
    drop(bytes_viewer);
    surface.kill();
}

/// One RING_ADVANCE waits per stream: 10k reads while the viewer does not
/// read leave one queued advance to the newest end, and the bytes up to it
/// are the ones parsed after the READY's cut.
#[test]
fn output_ring_attach_a_viewer_that_does_not_read_has_one_advance_waiting() {
    let (_mux, surface) = quiet_terminal("output-ring-one-advance");
    let pty = surface.as_pty().unwrap();
    let (stream, grant) = ring_attach(&surface);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();
    let ready = surface.take_viewer_snapshot(&stream.receiver, None).unwrap();

    let mut parsed = Vec::new();
    for index in 0..10_000 {
        parsed.extend(feed(pty, &line(index)[..8]));
        assert!(stream.receiver.queued_items() <= 1, "advance {index} did not merge");
    }
    assert_eq!(stream.receiver.queued_items(), 1);
    assert_eq!(stream.receiver.backlog_bytes(), size_of::<RingAdvance>());
    let interrupt = crate::stream_interrupt::StreamInterrupt::new();
    let ViewerEvent::Ring(advance) = stream.receiver.recv_viewer_event(&interrupt, None).unwrap()
    else {
        panic!("a ring viewer's output is an advance");
    };
    assert_eq!(advance, RingAdvance::new(ready.offset + parsed.len() as u64, 0, None));
    let mut in_ring = Vec::new();
    ring.read_into(ready.offset, advance.end, &mut in_ring).unwrap();
    assert_eq!(in_ring, parsed);
    drop(stream);
    surface.kill();
}

/// Colors apply after the bytes up to their own end, so an advance that
/// carries them takes no later end; an advance after them starts anew. A
/// grid change still reaches the viewer as a snapshot.
#[test]
fn output_ring_attach_colors_keep_their_end_and_a_resize_resyncs() {
    let lifecycle = AttachLifecycle::default();
    let (tap, receiver) = AttachTap::ring_pair(lifecycle, 16, 1 << 20);
    receiver.finish_snapshot_locked();
    let colors = Box::new(TerminalColors::default());
    assert!(tap.try_send_ring(RingAdvance::new(10, 0, None)));
    assert!(tap.try_send_ring(RingAdvance::new(20, 7, Some(colors.clone()))));
    assert!(tap.try_send_ring(RingAdvance::new(30, 0, None)));
    assert!(tap.try_send_ring(RingAdvance::new(40, 9, None)));
    let interrupt = crate::stream_interrupt::StreamInterrupt::new();
    let next = || receiver.recv_viewer_event(&interrupt, Some(Instant::now())).unwrap();
    assert!(
        matches!(next(), ViewerEvent::Ring(advance) if advance == RingAdvance::new(20, 7, Some(colors)))
    );
    assert!(
        matches!(next(), ViewerEvent::Ring(advance) if advance == RingAdvance::new(40, 9, None))
    );

    // Two echoes stay two advances.
    assert!(tap.try_send_ring(RingAdvance::new(50, 1, None)));
    assert!(tap.try_send_ring(RingAdvance::new(60, 2, None)));
    assert_eq!(receiver.queued_items(), 2);
    assert!(tap.try_send(AttachFrame::Resized {
        cols: 10,
        rows: 4,
        replay: Arc::from(&b""[..]),
        kitty_image_aliases: Vec::new(),
        kitty_state: KittyReplayState::disabled(),
        pending_sequence: Arc::from(&b""[..]),
    }));
    assert!(matches!(next(), ViewerEvent::Snapshot), "a resize resyncs a ring viewer");
}

/// A second ring viewer maps the same ring; a byte viewer beside ring
/// viewers still gets every byte.
#[test]
fn output_ring_attach_viewers_share_one_ring_and_byte_viewers_keep_their_bytes() {
    let (_mux, surface) = quiet_terminal("output-ring-shared");
    let pty = surface.as_pty().unwrap();
    let (_first, first) = ring_attach(&surface);
    let printed = feed(pty, b"hello\r\n");
    let (_second, second) = ring_attach(&surface);
    assert_eq!(second.base, first.base, "one ring per terminal");
    assert_eq!(second.end, first.end + printed.len() as u64);
    let bytes_viewer = surface.attach_stream().unwrap();
    let more = feed(pty, b"world\r\n");
    let mut received = Vec::new();
    drain_output(&bytes_viewer.stream, &mut received);
    assert_eq!(received, more);
    drop(bytes_viewer);
    surface.kill();
}

/// The hosted path publishes colored output as a frame: those bytes go into
/// the ring in output order with the plain bytes around them, the ring's end
/// stays the snapshot offset, and the advance that ends at the colored chunk
/// carries its colors.
#[test]
fn output_ring_attach_colored_frames_reach_the_ring_in_output_order() {
    let (_mux, surface) = quiet_terminal("output-ring-colored");
    let pty = surface.as_pty().unwrap();
    let (stream, grant) = ring_attach(&surface);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();
    let ready = surface.take_viewer_snapshot(&stream.receiver, None).unwrap();

    let mut published: Vec<u8> = Vec::new();
    let plain = b"plain before\r\n".to_vec();
    pty.broadcast_attach_output_flow(&plain, 0);
    published.extend(&plain);
    let colored = b"colored\r\n".to_vec();
    let colors = Box::new(TerminalColors::default());
    pty.broadcast_attach_frame(AttachFrame::output_with_colors(colored.clone(), colors.clone(), 0));
    published.extend(&colored);
    let colored_end = ready.offset + published.len() as u64;
    let after = b"plain after\r\n".to_vec();
    pty.broadcast_attach_output_flow(&after, 31);
    published.extend(&after);

    let end = ring.end();
    assert_eq!(end, ready.offset + published.len() as u64);
    assert_eq!(end, surface.snapshot_stream_position().unwrap().1, "the ring end is the offset");
    let mut in_ring = Vec::new();
    ring.read_into(ready.offset, end, &mut in_ring).unwrap();
    assert_eq!(in_ring, published, "the ring holds both paths' bytes in output order");

    let interrupt = crate::stream_interrupt::StreamInterrupt::new();
    let next = || stream.receiver.recv_viewer_event(&interrupt, Some(Instant::now())).unwrap();
    let colored_advance = RingAdvance::new(colored_end, 0, Some(colors));
    assert!(
        matches!(next(), ViewerEvent::Ring(advance) if advance == colored_advance),
        "the colors apply at the end of the colored chunk"
    );
    let last = RingAdvance::new(end, 31, None);
    assert!(matches!(next(), ViewerEvent::Ring(advance) if advance == last));
    drop(stream);
    surface.kill();
}

/// The terminal's end unmaps the daemon's writable view of its ring and no
/// ring is made again; a reader that mapped it before keeps every byte,
/// the exit's mode reset last.
#[test]
fn output_ring_attach_the_terminal_end_drops_the_ring_and_readers_keep_the_bytes() {
    let (_mux, surface) = quiet_terminal("output-ring-end");
    let pty = surface.as_pty().unwrap();
    let (stream, grant) = ring_attach(&surface);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();
    drop(grant);
    let printed = feed(pty, b"last words\r\n");
    assert!(pty.taps.lock().unwrap().ring.is_live());

    pty.publish_final_frame();

    assert!(!pty.taps.lock().unwrap().ring.is_live(), "the daemon unmapped its ring");
    let end = ring.end();
    assert_eq!(end, surface.snapshot_stream_position().unwrap().1, "the exit reset was published");
    let mut in_ring = Vec::new();
    ring.read_into(ring.base(), end, &mut in_ring).unwrap();
    assert_eq!(&in_ring[..printed.len()], &printed[..], "the reader's mapping keeps the bytes");
    let again =
        surface.attach_snapshot_ring_stream(AttachLifecycle::default(), 1 << 20, false).unwrap();
    assert!(
        matches!(again.ring, Err(snapshot_attach::RingUnavailable::Exited)),
        "an ended terminal makes no ring again"
    );
    drop((stream, again));
    surface.kill();
}

/// A hosted terminal's exit ends its ring the same way.
#[test]
fn output_ring_attach_a_hosted_exit_drops_the_ring() {
    let (_mux, surface) = quiet_terminal("output-ring-hosted-end");
    let pty = surface.as_pty().unwrap();
    let (stream, grant) = ring_attach(&surface);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();
    let printed = feed(pty, b"bye\r\n");

    pty.finish_hosted_exit();

    assert!(!pty.taps.lock().unwrap().ring.is_live(), "the daemon unmapped its ring");
    let mut in_ring = Vec::new();
    ring.read_into(grant.base, grant.base + printed.len() as u64, &mut in_ring).unwrap();
    assert_eq!(in_ring, printed);
    drop(stream);
    surface.kill();
}
