//! p05-ipc-111: a ring stream's frames. A terminal's ring viewer gets one
//! RING frame with a read-only descriptor of the ring, then one empty
//! RING_ADVANCE per merged end; the bytes the reader maps are the bytes the
//! daemon parsed, at the offsets the frames name.

use std::os::fd::AsFd;
use std::sync::Arc;

use cmux_ipc::output_ring::{DEFAULT_CAPACITY, LAYOUT, OutputRingReader};
use cmux_ipc::{Aux, FrameKind, FrameReader, Message, ReaderLimits};
use serde_json::{Value, json};

use super::{AttachFrameEncoder, AttachFrameError, BudgetedFrame};
use crate::server::AttachWireShape;
use crate::server::render_service::OutboundByteBudget;
use crate::stream_interrupt::StreamInterrupt;
use crate::surface::{AttachLifecycle, RingAdvance, ViewerEvent};
use crate::{Mux, Surface, SurfaceOptions, TerminalColors};

const STREAM: u16 = 5;
const SURFACE: u64 = 1;

fn read_back(frames: &[&BudgetedFrame]) -> Vec<(FrameKind, u16, Aux, Vec<u8>)> {
    let mut reader = FrameReader::new(ReaderLimits::default());
    reader.negotiate_frames();
    for frame in frames {
        reader.push(&frame.header().encode());
        reader.push(frame.payload());
    }
    let mut read = Vec::new();
    while let Some(message) = reader.next_message().unwrap() {
        let Message::Frame(frame) = message else { panic!("a JSON line among ring frames") };
        let header = *frame.header();
        read.push((header.kind(), header.stream(), header.aux(), frame.payload().to_vec()));
    }
    read
}

fn quiet_terminal(name: &str) -> (Arc<Mux>, Arc<Surface>) {
    let mux = Mux::new_for_test(name, SurfaceOptions::default());
    let options = SurfaceOptions {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "read _".into()]),
        ..SurfaceOptions::default()
    };
    let surface = Surface::spawn_for_test(SURFACE, options, Arc::downgrade(&mux)).unwrap();
    (mux, surface)
}

/// The whole path: ring attach, READY, output, then RING and RING_ADVANCE
/// frames read back through the cmux-ipc reader; the descriptor the RING
/// frame carries maps the parsed bytes at the advance's offsets and refuses
/// to map writable.
#[test]
fn output_ring_attach_frames_name_the_parsed_bytes() {
    let (_mux, surface) = quiet_terminal("output-ring-frames");
    let attach =
        surface.attach_snapshot_ring_stream(AttachLifecycle::default(), 1 << 20, false).unwrap();
    let (stream, grant) = (attach.stream, attach.ring.unwrap());
    let ready = surface.take_viewer_snapshot(&stream.receiver, None).unwrap();
    let mut parsed = Vec::new();
    for index in 0..300 {
        parsed.extend(surface.inject_output_for_test(format!("row {index}\r\n").as_bytes()));
    }
    let interrupt = StreamInterrupt::new();
    let ViewerEvent::Ring(advance) = stream.receiver.recv_viewer_event(&interrupt, None).unwrap()
    else {
        panic!("a ring viewer's output is an advance");
    };

    let budget = Arc::new(OutboundByteBudget::new(1 << 20));
    let mut encoder =
        AttachFrameEncoder::new(STREAM, SURFACE, AttachWireShape::default(), ready.offset).unwrap();
    let (base, end_at_grant) = (grant.base, grant.end);
    let announcement = encoder.ring(grant, &budget).unwrap();
    let encoded = encoder.ring_advance(advance, &budget).unwrap();
    assert!(encoded.sidecar.is_none());
    assert_eq!(encoder.offset(), ready.offset + parsed.len() as u64);

    let read = read_back(&[&announcement.frame, &encoded.frame]);
    assert_eq!(read.len(), 2);
    let (kind, stream_id, aux, meta) = &read[0];
    assert_eq!((*kind, *stream_id, *aux), (FrameKind::Ring, STREAM, Aux::Offset(end_at_grant)));
    let meta: Value = serde_json::from_slice(meta).unwrap();
    assert_eq!(meta, json!({"capacity": DEFAULT_CAPACITY, "base": base, "layout": LAYOUT}));
    let end = ready.offset + parsed.len() as u64;
    assert_eq!(read[1], (FrameKind::RingAdvance, STREAM, Aux::Offset(end), Vec::new()));

    let ring = OutputRingReader::open(announcement.descriptor.as_fd()).unwrap();
    let mut in_ring = Vec::new();
    ring.read_into(ready.offset, end, &mut in_ring).unwrap();
    assert_eq!(in_ring, parsed, "the ring holds the parsed bytes at the frames' offsets");
    // SAFETY: a probe mapping of a live descriptor, unmapped below if made.
    let writable = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            std::os::fd::AsRawFd::as_raw_fd(&announcement.descriptor),
            0,
        )
    };
    assert_eq!(writable, libc::MAP_FAILED, "the reader's descriptor cannot write the ring");
    drop(stream);
    surface.kill();
}

/// Colors and an echoed flow follow the advance as its `output` sidecar at
/// the same offset; a stream announces one ring, advances need it, and ends
/// only grow.
#[test]
fn output_ring_attach_advances_carry_their_sidecar_and_keep_the_stream_rules() {
    let (_mux, surface) = quiet_terminal("output-ring-rules");
    let attach =
        surface.attach_snapshot_ring_stream(AttachLifecycle::default(), 1 << 20, false).unwrap();
    let grant = attach.ring.unwrap();
    let second = surface
        .attach_snapshot_ring_stream(AttachLifecycle::default(), 1 << 20, false)
        .unwrap()
        .ring
        .unwrap();
    let budget = Arc::new(OutboundByteBudget::new(1 << 20));
    let mut encoder =
        AttachFrameEncoder::new(STREAM, SURFACE, AttachWireShape::default(), grant.end).unwrap();

    let Err(early) = encoder.ring_advance(RingAdvance::new(grant.end + 1, 0, None), &budget) else {
        panic!("an advance before the ring");
    };
    assert!(matches!(early.error, AttachFrameError::NoRing));
    encoder.ring(grant, &budget).unwrap();
    assert!(matches!(encoder.ring(second, &budget), Err(AttachFrameError::RingAgain)));

    let colors = TerminalColors::default();
    let end = encoder.offset() + 4;
    let encoded =
        encoder.ring_advance(RingAdvance::new(end, 42, Some(Box::new(colors))), &budget).unwrap();
    let sidecar = encoded.sidecar.as_ref().expect("colors and flow ride a sidecar");
    assert_eq!(sidecar.header().kind(), FrameKind::Event);
    assert_eq!(sidecar.offset(), Some(end), "the sidecar names the advance's end");
    let event: Value = serde_json::from_slice(sidecar.payload()).unwrap();
    assert_eq!(event["event"], "output");
    assert_eq!(event["flow"], 42);
    assert!(event.get("colors").is_some());
    assert!(event.get("data").is_none(), "a ring advance carries no bytes");

    let Err(behind) = encoder.ring_advance(RingAdvance::new(end - 1, 0, None), &budget) else {
        panic!("an advance behind the offset");
    };
    assert!(matches!(behind.error, AttachFrameError::RingBehind { .. }));
    assert_eq!(behind.advance.end, end - 1, "a refused advance comes back whole");
    assert_eq!(encoder.offset(), end);
    drop(attach.stream);
    surface.kill();
}
