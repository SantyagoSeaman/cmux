//! p05-ipc-114: resume by offset on reattach. A ring viewer that names the
//! offset it parsed and its screen's generation gets RING, then RING_ADVANCE
//! from that offset, and no READY, while the terminal's ring still holds the
//! offset and the grid is unchanged; otherwise its first event is a READY.

use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cmux_ipc::output_ring::{DEFAULT_CAPACITY, OutputRingReader};
use cmux_ipc::{Aux, FrameKind, FrameReader, Message, ReaderLimits};
use serde_json::Value;

use super::ring_resume;
use crate::server::AttachWireShape;
use crate::server::attach_frames::{AttachFrameEncoder, BudgetedFrame};
use crate::server::render_service::OutboundByteBudget;
use crate::stream_interrupt::StreamInterrupt;
use crate::surface::snapshot_attach::ring_resume::{ResumeRefused, RingResume, RingResumeAttach};
use crate::surface::snapshot_attach::{SnapshotAdmission, SnapshotAttachStream};
use crate::surface::{AttachLifecycle, ViewerEvent};
use crate::{Mux, Surface, SurfaceOptions};

const STREAM: u16 = 7;
const SURFACE: u64 = 1;
const BACKLOG: usize = 1 << 20;

/// A terminal whose program prints nothing, so every output byte is one a
/// test fed.
fn quiet_terminal(name: &str) -> (Arc<Mux>, Arc<Surface>) {
    let mux = Mux::new_for_test(name, SurfaceOptions::default());
    let options = SurfaceOptions {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "read _".into()]),
        ..SurfaceOptions::default()
    };
    let surface = Surface::spawn_for_test(SURFACE, options, Arc::downgrade(&mux)).unwrap();
    (mux, surface)
}

fn feed(surface: &Surface, lines: std::ops::Range<usize>) -> Vec<u8> {
    let mut parsed = Vec::new();
    for index in lines {
        parsed.extend(surface.inject_output_for_test(format!("row {index:07}\r\n").as_bytes()));
    }
    parsed
}

fn next_event(stream: &SnapshotAttachStream) -> ViewerEvent {
    let deadline = Some(Instant::now() + Duration::from_secs(5));
    stream.receiver.recv_viewer_event(&StreamInterrupt::new(), deadline).unwrap()
}

fn read_back(frames: &[&BudgetedFrame]) -> Vec<(FrameKind, Aux, Vec<u8>)> {
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
        read.push((header.kind(), header.aux(), frame.payload().to_vec()));
    }
    read
}

/// A first ring viewer that took its READY and parsed `lines`; the screen it
/// leaves with.
fn first_viewer(surface: &Surface, lines: std::ops::Range<usize>) -> RingResume {
    let attach = surface.attach_snapshot_ring_stream(AttachLifecycle::default(), BACKLOG, false);
    let attach = attach.unwrap();
    assert!(attach.ring.is_ok(), "the first ring attach makes the ring");
    let ready = surface.take_viewer_snapshot(&attach.stream.receiver, None).unwrap();
    let parsed = feed(surface, lines);
    RingResume { offset: ready.offset + parsed.len() as u64, generation: ready.generation }
}

/// Inside the ring: the stream's frames are RING and RING_ADVANCE from the
/// resume offset (plus the colors sidecar), never a SNAPSHOT, and the ring
/// holds the bytes the viewer missed at the offsets the frames name.
#[test]
fn resume_offset_inside_the_ring_sends_ring_and_advance_without_a_ready() {
    let (_mux, surface) = quiet_terminal("resume-offset-inside");
    let resume = first_viewer(&surface, 0..100);
    let missed = feed(&surface, 100..300);

    let attach = surface
        .resume_snapshot_ring_stream(AttachLifecycle::default(), BACKLOG, false, resume)
        .unwrap();
    let RingResumeAttach::Resumed { stream, ring, from } = attach else {
        panic!("a resume inside the ring was refused");
    };
    assert_eq!(from, resume.offset);
    let ViewerEvent::Ring(advance) = next_event(&stream) else {
        panic!("a resumed viewer's first event is its advance, not a READY");
    };
    let end = resume.offset + missed.len() as u64;
    assert_eq!((advance.end, ring.end), (end, end));
    assert!(advance.colors.is_some(), "the advance restates the colors");

    let budget = Arc::new(OutboundByteBudget::new(1 << 20));
    let mut encoder =
        AttachFrameEncoder::new(STREAM, SURFACE, AttachWireShape::default(), from).unwrap();
    let announcement = encoder.ring(ring, &budget).unwrap();
    let encoded = encoder.ring_advance(advance, &budget).unwrap();
    let sidecar = encoded.sidecar.as_ref().expect("colors ride the advance's sidecar");
    let read = read_back(&[&announcement.frame, &encoded.frame, sidecar]);
    let kinds: Vec<_> = read.iter().map(|(kind, aux, _)| (*kind, *aux)).collect();
    assert_eq!(
        kinds,
        [
            (FrameKind::Ring, Aux::Offset(end)),
            (FrameKind::RingAdvance, Aux::Offset(end)),
            (FrameKind::Event, Aux::Offset(end)),
        ]
    );
    let event: Value = serde_json::from_slice(&read[2].2).unwrap();
    assert_eq!(event["event"], "output");

    let reader = OutputRingReader::open(announcement.descriptor.as_fd()).unwrap();
    let mut in_ring = Vec::new();
    reader.read_into(from, end, &mut in_ring).unwrap();
    assert_eq!(in_ring, missed, "the ring holds what the viewer missed, from its offset");

    let later = feed(&surface, 300..310);
    let ViewerEvent::Ring(next) = next_event(&stream) else { panic!("live output follows") };
    assert_eq!(next.end, end + later.len() as u64);
    // No READY was pending, so a request is admitted, not collapsed into one.
    assert_eq!(stream.requests.request(Instant::now()), SnapshotAdmission::Accepted);
    assert!(matches!(next_event(&stream), ViewerEvent::Snapshot));
    drop(stream);
    surface.kill();
}

/// Resumed at the ring's end (nothing missed): still no READY, and an empty
/// advance.
#[test]
fn resume_offset_at_the_ring_end_resumes_with_an_empty_advance() {
    let (_mux, surface) = quiet_terminal("resume-offset-end");
    let resume = first_viewer(&surface, 0..20);
    let attach = surface
        .resume_snapshot_ring_stream(AttachLifecycle::default(), BACKLOG, false, resume)
        .unwrap();
    let RingResumeAttach::Resumed { stream, .. } = attach else { panic!("refused at the end") };
    let ViewerEvent::Ring(advance) = next_event(&stream) else { panic!("not an advance") };
    assert_eq!(advance.end, resume.offset);
    drop(stream);
    surface.kill();
}

/// Outside the ring, past its end, at another generation, or with no ring:
/// an ordinary ring attach whose first event is a READY.
#[test]
fn resume_offset_outside_the_ring_gets_a_ready() {
    let (mux, surface) = quiet_terminal("resume-offset-outside");
    let fresh = Surface::spawn_for_test(
        SURFACE + 1,
        SurfaceOptions {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "read _".into()]),
            ..SurfaceOptions::default()
        },
        Arc::downgrade(&mux),
    )
    .unwrap();
    let resume = first_viewer(&surface, 0..10);
    let ahead = RingResume { offset: resume.offset + 1, ..resume };
    let regrid = RingResume { generation: resume.generation + 1, ..resume };
    let never = RingResume { offset: 0, generation: 0 };

    let refused = |surface: &Surface, resume: RingResume| -> ResumeRefused {
        let attach = surface
            .resume_snapshot_ring_stream(AttachLifecycle::default(), BACKLOG, false, resume)
            .unwrap();
        let RingResumeAttach::Ready { attach, why } = attach else {
            panic!("{resume:?} resumed");
        };
        assert!(attach.ring.is_ok(), "the READY attach still gets the ring");
        assert!(matches!(next_event(&attach.stream), ViewerEvent::Snapshot), "READY first");
        why
    };
    assert!(matches!(refused(&surface, ahead), ResumeRefused::OutsideRing { .. }));
    assert!(matches!(refused(&surface, regrid), ResumeRefused::Generation { .. }));
    assert!(matches!(refused(&fresh, never), ResumeRefused::NoRing));

    // More than the ring's capacity after the offset: the oldest byte the
    // viewer needs was overwritten.
    let mut written = 0;
    let mut index = 10;
    while written <= DEFAULT_CAPACITY {
        written += feed(&surface, index..index + 1000).len();
        index += 1000;
    }
    let ResumeRefused::OutsideRing { offset, oldest, .. } = refused(&surface, resume) else {
        panic!("a lapped offset resumed");
    };
    assert!(offset < oldest);
    surface.kill();
    fresh.kill();
}

/// The two fields come together or not at all, on every attach.
#[test]
fn resume_offset_and_generation_come_together() {
    assert_eq!(ring_resume(None, None).unwrap(), None);
    assert_eq!(
        ring_resume(Some(5), Some(2)).unwrap(),
        Some(RingResume { offset: 5, generation: 2 })
    );
    for (offset, generation) in [(Some(5), None), (None, Some(2))] {
        let error = ring_resume(offset, generation).unwrap_err().to_string();
        assert!(error.contains("must be supplied together"), "{error}");
    }
}
