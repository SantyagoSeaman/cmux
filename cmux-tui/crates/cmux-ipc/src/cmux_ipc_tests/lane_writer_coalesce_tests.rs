//! Producer-side coalescing of LaneWriter (p05-ipc-141): queued unsent items
//! with the same key are replaced (additive fields summed), a merge passes
//! only keyed items, and every unkeyed item keeps its place.

use std::collections::VecDeque;

use bytes::Bytes;

use super::lane_writer_tests::Rng;
use crate::{
    Aux, CallAux, CallFlags, CoalesceKey, Frame, FrameKind, LaneWriter, PriorityHint, Pushed,
    StreamLane,
};

/// The `size-state` event id (spec/sdk-schema.json).
const SIZE_STATE: u16 = 0xC00B;
/// `resize-attached-view` (spec/sdk-schema.json), an input-priority method.
const RESIZE: u16 = 0x0502;

/// A call frame of `kind` on `stream` whose request id names the message and
/// whose payload is `value` (a generation, a size, a delta).
fn call_frame(kind: FrameKind, stream: u16, method_id: u16, message: u32, value: u32) -> Frame {
    let priority = PriorityHint::Control;
    let aux =
        Aux::Call(CallAux { request_id: message, method_id, priority, flags: CallFlags::empty() });
    Frame::new(kind, stream, aux, Bytes::copy_from_slice(&value.to_le_bytes())).unwrap()
}

fn size_state(message: u32) -> Frame {
    call_frame(FrameKind::TypedEvent, 0, SIZE_STATE, message, message)
}

fn reply(message: u32) -> Frame {
    call_frame(FrameKind::Reply, 0, 0x0101, message, 0)
}

fn value(frame: &Frame) -> u32 {
    let bytes: [u8; 4] = frame.payload().as_ref().try_into().unwrap();
    u32::from_le_bytes(bytes)
}

fn request_id(frame: &Frame) -> u32 {
    frame.header().call().unwrap().request_id
}

fn keep_newer(_: &Frame, newer: Frame) -> Frame {
    newer
}

/// The schema's additive merge for a payload that is one summed field.
fn sum_delta(queued: &Frame, newer: Frame) -> Frame {
    let header = newer.header();
    let total = value(queued).wrapping_add(value(&newer));
    let payload = Bytes::copy_from_slice(&total.to_le_bytes());
    Frame::new(header.kind(), header.stream(), header.aux(), payload).unwrap()
}

#[test]
fn lane_writer_coalesce_1000_size_states_behind_a_blocked_writer_send_1() {
    let mut writer = LaneWriter::default();
    let (surface_a, surface_b) = (CoalesceKey::new(SIZE_STATE, 7), CoalesceKey::new(SIZE_STATE, 8));
    writer.push_control(reply(1)).unwrap();
    // Nothing pops: the socket is blocked while the producer publishes.
    for generation in 0..1000 {
        let pushed =
            writer.push_control_coalescing(size_state(10 + generation), surface_a, keep_newer);
        assert_eq!(pushed, Ok(if generation == 0 { Pushed::Queued } else { Pushed::Coalesced }));
        // Another surface's state between them is passed: their keys differ.
        let other = size_state(5000 + generation);
        let pushed = writer.push_control_coalescing(other, surface_b, keep_newer);
        assert_eq!(pushed, Ok(if generation == 0 { Pushed::Queued } else { Pushed::Coalesced }));
    }
    let sent: Vec<u32> = std::iter::from_fn(|| writer.pop()).map(|frame| value(&frame)).collect();
    assert_eq!(sent, vec![0, 1009, 5999], "the reply, then one newest state per surface");
    assert_eq!(writer.queued_bytes(), 0);
}

#[test]
fn lane_writer_coalesce_never_passes_an_unkeyed_item() {
    let mut writer = LaneWriter::default();
    let key = CoalesceKey::new(SIZE_STATE, 7);
    writer.push_control_coalescing(size_state(1), key, keep_newer).unwrap();
    writer.push_control(reply(2)).unwrap();
    assert_eq!(writer.push_control_coalescing(size_state(3), key, keep_newer), Ok(Pushed::Queued));
    assert_eq!(
        writer.push_control_coalescing(size_state(4), key, keep_newer),
        Ok(Pushed::Coalesced)
    );
    // The same key on another control-sequence stream is another item.
    let on_nine = call_frame(FrameKind::TypedEvent, 9, SIZE_STATE, 5, 5);
    assert_eq!(writer.push_control_coalescing(on_nine, key, keep_newer), Ok(Pushed::Queued));
    let sent: Vec<(u16, u32)> = std::iter::from_fn(|| writer.pop())
        .map(|frame| (frame.header().stream(), request_id(&frame)))
        .collect();
    assert_eq!(sent, vec![(0, 1), (0, 2), (0, 4), (9, 5)]);
}

#[test]
fn lane_writer_coalesce_100_resizes_in_a_frame_send_1_and_deltas_sum() {
    let mut writer = LaneWriter::default();
    writer.open_stream(3, StreamLane::Input).unwrap();
    let resize = CoalesceKey::new(RESIZE, 3);
    let scroll = CoalesceKey::new(0x0503, 3);
    for size in 1..=100 {
        let frame = call_frame(FrameKind::Request, 3, RESIZE, size, size);
        writer.push_stream_coalescing(frame, resize, keep_newer).unwrap();
        let delta = call_frame(FrameKind::Request, 3, 0x0503, 1000 + size, 1);
        writer.push_stream_coalescing(delta, scroll, sum_delta).unwrap();
    }
    let sent: Vec<(u32, u32)> = std::iter::from_fn(|| writer.pop())
        .map(|frame| (request_id(&frame), value(&frame)))
        .collect();
    assert_eq!(sent, vec![(100, 100), (1100, 100)], "the newest size, and the summed delta");
    assert_eq!(writer.queued_bytes(), 0);
}

/// Seeded: keyed and unkeyed items on two control-sequence streams, random
/// pops in between. The wire equals a model where a keyed push replaces the
/// newest same-key, same-stream item in the trailing run of keyed items, and
/// the unkeyed items leave in push order with the same keyed items around them.
#[test]
fn lane_writer_coalesce_keeps_order_with_unkeyed_items() {
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut writer = LaneWriter::default();
        // (stream, key, message) of every unsent item, in FIFO order.
        let mut model: VecDeque<(u16, Option<CoalesceKey>, u32)> = VecDeque::new();
        let (mut written, mut expected) = (Vec::new(), Vec::new());
        for message in 1..500_u32 {
            let stream = if rng.below(3) == 0 { 9 } else { 0 };
            let frame = call_frame(FrameKind::TypedEvent, stream, SIZE_STATE, message, message);
            match rng.below(5) {
                0 => {
                    assert_eq!(writer.push_control(frame), Ok(Pushed::Queued));
                    model.push_back((stream, None, message));
                }
                1..=3 => {
                    let key = CoalesceKey::new(SIZE_STATE, rng.below(3));
                    let pushed = writer.push_control_coalescing(frame, key, keep_newer);
                    let run = model.iter_mut().rev().take_while(|item| item.1.is_some());
                    match run.into_iter().find(|item| item.0 == stream && item.1 == Some(key)) {
                        Some(queued) => {
                            assert_eq!(pushed, Ok(Pushed::Coalesced), "seed {seed}");
                            queued.2 = message;
                        }
                        None => {
                            assert_eq!(pushed, Ok(Pushed::Queued), "seed {seed}");
                            model.push_back((stream, Some(key), message));
                        }
                    }
                }
                _ => {
                    for _ in 0..rng.below(3) {
                        let Some(frame) = writer.pop() else { break };
                        written.push(request_id(&frame));
                        expected.push(model.pop_front().unwrap().2);
                    }
                }
            }
        }
        while let Some(frame) = writer.pop() {
            written.push(request_id(&frame));
            expected.push(model.pop_front().unwrap().2);
        }
        assert_eq!(written, expected, "seed {seed}");
        assert!(model.is_empty());
        assert_eq!(writer.queued_bytes(), 0);
    }
}
