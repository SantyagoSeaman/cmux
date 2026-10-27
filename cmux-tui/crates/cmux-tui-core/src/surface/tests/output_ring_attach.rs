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
    let (stream, grant) = ring_attach(&surface);
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
        // The ring viewer's app parsed it all (p05-ipc-119: else the ring skips).
        surface.ring_ack(&stream.receiver, ring.end());
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
    drop((bytes_viewer, stream));
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
    let (tap, receiver) = AttachTap::ring_pair(lifecycle, 16, 1 << 20, None);
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

/// p05-ipc-119, flood while holding: the app holds the span right after its
/// READY (Ghostty parsing it) and acks nothing while the program floods past
/// the ring. The daemon never overwrites the held span: it skips, gaps the
/// viewer once and holds its READY (an app `snapshot-request` included)
/// until the app acks the gap. The READY then continues the ring exactly.
#[test]
fn output_ring_attach_a_held_span_is_skipped_never_overwritten() {
    use cmux_ipc::output_ring::{RingAck, SpanError};
    use snapshot_attach::SnapshotAdmission;
    let (_mux, surface) = quiet_terminal("output-ring-ack-flood");
    let pty = surface.as_pty().unwrap();
    let (stream, grant) = ring_attach(&surface);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();
    let ready = surface.take_viewer_snapshot(&stream.receiver, None).unwrap();
    let held = feed(pty, b"held span\r\n");
    let held_from = ready.offset;
    let held_to = held_from + held.len() as u64;
    ring.cut(held_from, held_to).unwrap();
    assert_eq!(surface.ring_ack(&stream.receiver, held_from), RingAck::Stale);

    let mut index = 0;
    while ring.base() == grant.base {
        index += 1;
        feed(pty, &line(index));
        assert!(index < 100_000, "the ring never skipped");
    }
    let skips = surface.ring_skips_for_test();
    for _ in 0..2_000 {
        index += 1;
        feed(pty, &line(index));
    }
    assert!(surface.ring_skips_for_test() > skips, "it keeps skipping, it never waits");
    // The writer never reserved past the held span's slots; the ring's base
    // moved past it, so nothing cuts it again.
    assert_eq!(ring.check(held_from), Ok(()), "the held span was never overwritten");
    let end = ring.end();
    assert!(matches!(ring.cut(held_to, end), Err(SpanError::BeforeBase { .. })));

    // The gap holds the READY, whatever asks for it.
    assert!(stream.receiver.ring_gap_pending());
    assert_eq!(stream.receiver.resyncs(), 1);
    // The app's `snapshot-request` for the gap is answered by that resync.
    stream.requests.sent(Instant::now() - Duration::from_secs(1));
    assert_eq!(stream.requests.request(Instant::now()), SnapshotAdmission::Collapsed);
    assert!(stream.receiver.ring_gap_pending());
    let interrupt = crate::stream_interrupt::StreamInterrupt::new();
    let now = || Some(Instant::now());
    assert!(matches!(
        stream.receiver.recv_viewer_event(&interrupt, now()),
        Err(RecvTimeoutError::Timeout)
    ));
    // The held span is parsed; the app still holds a later one: not the gap yet.
    assert_eq!(surface.ring_ack(&stream.receiver, held_to), RingAck::Moved);
    assert!(stream.receiver.ring_gap_pending());
    // It holds nothing: it acks the ring's base, and the READY goes.
    assert_eq!(surface.ring_ack(&stream.receiver, ring.base()), RingAck::GapAcked);
    assert!(matches!(
        stream.receiver.recv_viewer_event(&interrupt, now()),
        Ok(ViewerEvent::Snapshot)
    ));
    let resync = surface.take_ring_viewer_snapshot(&stream.receiver, None).unwrap().unwrap();
    assert_eq!(stream.receiver.resyncs(), 1, "one gap, one resync");
    assert!(resync.offset >= end);

    let after = feed(pty, b"after the gap\r\n");
    let ViewerEvent::Ring(advance) = stream.receiver.recv_viewer_event(&interrupt, now()).unwrap()
    else {
        panic!("a ring viewer's output is an advance");
    };
    assert_eq!(advance.end, resync.offset + after.len() as u64);
    let mut continued = Vec::new();
    ring.read_into(resync.offset, advance.end, &mut continued).unwrap();
    assert_eq!(continued, after, "the READY continues the ring exactly");
    drop(stream);
    surface.kill();
}

/// A viewer whose app acks what it parsed is never gapped however much the
/// program prints, and a ring viewer that detached holds nothing.
#[test]
fn output_ring_attach_an_acking_viewer_never_gaps_and_a_gone_one_holds_nothing() {
    use cmux_ipc::output_ring::RingAck;
    let (_mux, surface) = quiet_terminal("output-ring-ack-keeps-up");
    let pty = surface.as_pty().unwrap();
    let (stream, grant) = ring_attach(&surface);
    let (idle, _idle_grant) = ring_attach(&surface);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();
    let ready = surface.take_viewer_snapshot(&stream.receiver, None).unwrap();
    drop(idle); // gone without an ack: its cursor closes with its tap
    let interrupt = crate::stream_interrupt::StreamInterrupt::new();
    let (mut parsed, mut index) = (ready.offset, 0);
    while parsed < ready.offset + 3 * DEFAULT_CAPACITY as u64 {
        for _ in 0..50 {
            index += 1;
            feed(pty, &line(index));
        }
        let Ok(ViewerEvent::Ring(advance)) =
            stream.receiver.recv_viewer_event(&interrupt, Some(Instant::now()))
        else {
            panic!("an advance");
        };
        let mut bytes = Vec::new();
        ring.read_into(parsed, advance.end, &mut bytes).unwrap();
        parsed = advance.end;
        assert_eq!(surface.ring_ack(&stream.receiver, parsed), RingAck::Moved);
    }
    assert_eq!(surface.ring_skips_for_test(), 0, "a viewer that keeps up is never skipped");
    assert_eq!(stream.receiver.resyncs(), 0);
    drop(stream);
    surface.kill();
}

/// What a ring viewer's worker does with its next events, and its app with
/// what arrives: READYs are taken and counted, advances read from the ring.
fn serve_ring_viewer(
    surface: &Surface,
    receiver: &AttachFrameReceiver,
    ring: &OutputRingReader,
    parsed: &mut u64,
) -> usize {
    let interrupt = crate::stream_interrupt::StreamInterrupt::new();
    let mut readies = 0;
    while let Ok(event) = receiver.recv_viewer_event(&interrupt, Some(Instant::now())) {
        match event {
            ViewerEvent::Snapshot => {
                if let Some(ready) = surface.take_ring_viewer_snapshot(receiver, None).unwrap() {
                    readies += 1;
                    *parsed = ready.offset;
                }
            }
            ViewerEvent::Ring(advance) => {
                let mut bytes = Vec::new();
                if ring.read_into(*parsed, advance.end, &mut bytes).is_ok() {
                    *parsed = advance.end;
                }
            }
            ViewerEvent::Frame(_) | ViewerEvent::LocalReady(_) => {}
        }
    }
    readies
}

/// p05-ipc-119, two viewers: A's app holds a span and never acks (hung), B's
/// app parses what arrives and acks every frame (holding nothing, it acks
/// the ring's base). B is gapped by the stall A causes and gets exactly one
/// READY for it, when the stall ends (A's hold lapses), not one per ack round
/// trip; that READY continues the ring.
#[test]
fn output_ring_attach_one_stall_is_one_resync_for_a_healthy_viewer() {
    use cmux_ipc::output_ring::GAP_HOLD_CAPACITIES;
    let (_mux, surface) = quiet_terminal("output-ring-two-viewers");
    let pty = surface.as_pty().unwrap();
    let (hung, hung_grant) = ring_attach(&surface);
    let (healthy, grant) = ring_attach(&surface);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();
    let hung_ring = OutputRingReader::open(hung_grant.descriptor.as_fd()).unwrap();
    let hung_ready = surface.take_ring_viewer_snapshot(&hung.receiver, None).unwrap().unwrap();
    let mut parsed = 0;
    assert_eq!(serve_ring_viewer(&surface, &healthy.receiver, &ring, &mut parsed), 1);
    let held = feed(pty, b"held forever\r\n");
    hung_ring.cut(hung_ready.offset, hung_ready.offset + held.len() as u64).unwrap();

    let (mut readies, mut index, mut fed) = (0, 0, 0u64);
    let mut saw_stall = false;
    while !saw_stall || hung.receiver.ring_gap_pending() {
        for _ in 0..200 {
            index += 1;
            fed += feed(pty, &line(index)).len() as u64;
        }
        saw_stall |= hung.receiver.ring_gap_pending();
        readies += serve_ring_viewer(&surface, &healthy.receiver, &ring, &mut parsed);
        surface.ring_ack(&healthy.receiver, parsed.max(ring.base()));
        assert!(fed < 4 * GAP_HOLD_CAPACITIES * DEFAULT_CAPACITY as u64, "the hold never lapsed");
    }
    assert!(surface.ring_skips_for_test() > 1, "the stall took many skips");
    readies += serve_ring_viewer(&surface, &healthy.receiver, &ring, &mut parsed);
    assert_eq!(readies, 1, "one READY for the healthy viewer per stall");
    assert_eq!(healthy.receiver.resyncs(), 1);

    let after = feed(pty, b"after the stall\r\n");
    let from = parsed;
    serve_ring_viewer(&surface, &healthy.receiver, &ring, &mut parsed);
    assert_eq!(parsed, from + after.len() as u64, "the READY continues the ring");
    drop((hung, healthy));
    surface.kill();
}

/// p05-ipc-119: a gap that lands after the worker got its Snapshot event and
/// before it took the snapshot keeps the READY pending (a READY then would be
/// passed by the stall's next skip and cost a second resync). The one READY
/// goes once the app acked the gap, at or past the ring's base.
#[test]
fn output_ring_attach_a_gap_before_the_snapshot_is_taken_holds_the_ready() {
    use cmux_ipc::output_ring::RingAck;
    let (_mux, surface) = quiet_terminal("output-ring-gap-race");
    let pty = surface.as_pty().unwrap();
    let (stream, grant) = ring_attach(&surface);
    let ring = OutputRingReader::open(grant.descriptor.as_fd()).unwrap();
    let interrupt = crate::stream_interrupt::StreamInterrupt::new();
    assert!(matches!(
        stream.receiver.recv_viewer_event(&interrupt, Some(Instant::now())),
        Ok(ViewerEvent::Snapshot)
    ));
    // Output past the cursor's capacity before the worker takes the lock.
    let mut index = 0;
    while ring.base() == grant.base {
        index += 1;
        feed(pty, &line(index));
        assert!(index < 100_000, "the ring never skipped");
    }
    assert!(stream.receiver.ring_gap_pending());
    assert!(surface.take_ring_viewer_snapshot(&stream.receiver, None).unwrap().is_none());
    assert!(matches!(
        stream.receiver.recv_viewer_event(&interrupt, Some(Instant::now())),
        Err(RecvTimeoutError::Timeout)
    ));
    assert_eq!(surface.ring_ack(&stream.receiver, ring.base()), RingAck::GapAcked);
    let mut parsed = 0;
    assert_eq!(serve_ring_viewer(&surface, &stream.receiver, &ring, &mut parsed), 1);
    assert!(parsed >= ring.base(), "the READY is past the hole");
    let after = feed(pty, b"after the gap\r\n");
    let from = parsed;
    serve_ring_viewer(&surface, &stream.receiver, &ring, &mut parsed);
    assert_eq!(parsed, from + after.len() as u64, "its advance cuts");
    assert_eq!(surface.ring_skips_for_test(), 1, "one skip: nothing held the writer after it");
    drop(stream);
    surface.kill();
}
