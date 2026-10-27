//! One encodings slot per emit, shared by every wire subscriber that took
//! the emit (p05-ipc-148): the first of an encode class serializes, the rest
//! reuse its bytes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

fn bell(receiver: &MuxEventReceiver) -> DeliveredEvent {
    let interrupt = crate::stream_interrupt::StreamInterrupt::new();
    receiver.recv_delivered_until_interrupted(&interrupt).expect("a delivered bell")
}

fn encoded(name: &str) -> Option<EncodedEvent> {
    Some(EncodedEvent { name: name.into(), json: Arc::from(&b"{}"[..]) })
}

#[test]
fn wire_subscribers_of_one_emit_share_one_encodings_slot() {
    let bus = MuxEventBroadcaster::default();
    let wire: Vec<MuxEventReceiver> = (0..10).map(|_| bus.subscribe()).collect();
    for receiver in &wire {
        receiver.share_encodings();
    }
    let internal = bus.subscribe();
    bus.emit(MuxEvent::Bell(1));

    let delivered: Vec<DeliveredEvent> = wire.iter().map(bell).collect();
    let first = delivered[0].encodings.clone().expect("a wire subscriber shares encodings");
    assert!(delivered.iter().all(|delivered| {
        delivered.encodings.as_ref().is_some_and(|slot| Arc::ptr_eq(slot, &first))
    }));
    assert!(bell(&internal).encodings.is_none(), "a subscriber that did not ask gets no slot");

    let encodes = AtomicUsize::new(0);
    let plain = EncodeClass { tree_deltas: false, cause: None };
    let caused = EncodeClass { tree_deltas: false, cause: Some(EventCause::Automation) };
    for (index, delivered) in delivered.iter().enumerate() {
        let class = if index % 2 == 0 { plain } else { caused };
        let slot = delivered.encodings.as_ref().unwrap();
        let line = slot.get_or_encode(class, || {
            encodes.fetch_add(1, Ordering::Relaxed);
            encoded("bell")
        });
        assert_eq!(line.map(|line| &*line.name), Some("bell"));
    }
    assert_eq!(encodes.load(Ordering::Relaxed), 2, "one encode per class");
}

/// Each emit gets its own slot: a later event is never written with an
/// earlier one's bytes.
#[test]
fn each_emit_has_its_own_slot() {
    let bus = MuxEventBroadcaster::default();
    let receiver = bus.subscribe();
    receiver.share_encodings();
    bus.emit(MuxEvent::Bell(1));
    bus.emit(MuxEvent::Bell(2));
    let (first, second) = (bell(&receiver), bell(&receiver));
    assert!(!Arc::ptr_eq(first.encodings.as_ref().unwrap(), second.encodings.as_ref().unwrap()));
}
