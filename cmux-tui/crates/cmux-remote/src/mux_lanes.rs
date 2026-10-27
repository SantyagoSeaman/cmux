//! The wire lane of each mux message over cmux-remote (p05-ipc task 152).
//!
//! The routes come from the schema: cmux-ipc's build.rs computes one per
//! framed command and event from spec/sdk-schema.json and the reviewed
//! exceptions in cmux-ipc/remote-lanes.json (`cmux_ipc::RemoteLanes`). This
//! file only maps the schema's priority vocabulary onto the wire lanes and
//! reads the one or two envelope fields a route may be refined on.
//!
//! A JSON line costs one borrowed envelope parse. A binary frame (first byte
//! 0xFF, which never starts a UTF-8 line) is routed by the method or event id
//! in its header and is never parsed, as Mojo routes a message by its header
//! ordinal (chromium mojo/core/channel.cc, `Channel::Message::Deserialize`).

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use cmux_ipc::methods::{EVENT_FAMILIES, EventId, MethodId};
use cmux_ipc::{FrameHeader, FrameKind, HEADER_LEN, MAGIC, PriorityHint, RefineField, RemoteLanes};
use cmux_remote_protocol::Lane;
use serde::Deserialize;

const MAX_TRACKED_REQUESTS: usize = 4096;

/// The wire lane of a schema priority. `background` rides bulk, the lowest
/// lane: protocol-5 peers know only the four lanes of `Lane::ALL`.
pub(crate) const fn lane_of(priority: PriorityHint) -> Lane {
    match priority {
        PriorityHint::Input => Lane::Interactive,
        PriorityHint::Control => Lane::Control,
        PriorityHint::Bulk | PriorityHint::Background => Lane::Bulk,
    }
}

#[derive(Default)]
pub(crate) struct MuxLaneTracker {
    state: Mutex<MuxLaneState>,
}

#[derive(Default)]
struct MuxLaneState {
    requests: HashMap<u64, TrackedResponse>,
    order: VecDeque<(u64, u64)>,
    next_token: u64,
}

#[derive(Clone, Copy)]
struct TrackedResponse {
    disposition: ResponseDisposition,
    token: u64,
}

#[derive(Clone, Copy)]
enum ResponseDisposition {
    Forward(Lane),
    Suppress,
}

struct MuxName<'a>(Cow<'a, str>);

impl MuxName<'_> {
    fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for MuxName<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct MuxNameVisitor<'a>(std::marker::PhantomData<&'a ()>);

        impl<'de: 'a, 'a> serde::de::Visitor<'de> for MuxNameVisitor<'a> {
            type Value = MuxName<'a>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a mux protocol name")
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MuxName(Cow::Borrowed(value)))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MuxName(Cow::Owned(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MuxName(Cow::Owned(value)))
            }
        }

        deserializer.deserialize_str(MuxNameVisitor(std::marker::PhantomData))
    }
}

#[derive(Deserialize)]
struct MuxEnvelope<'a> {
    id: Option<u64>,
    #[serde(borrow)]
    cmd: Option<MuxName<'a>>,
    #[serde(borrow)]
    event: Option<MuxName<'a>>,
    #[serde(borrow)]
    scope: Option<MuxName<'a>>,
    #[serde(borrow)]
    mode: Option<MuxName<'a>>,
    #[serde(borrow)]
    op: Option<MuxName<'a>>,
}

impl MuxEnvelope<'_> {
    fn field(&self, field: RefineField) -> Option<&str> {
        match field {
            RefineField::Op => self.op.as_ref(),
            RefineField::Mode => self.mode.as_ref(),
            RefineField::Scope => self.scope.as_ref(),
        }
        .map(MuxName::as_str)
    }
}

fn parse_envelope(line: &[u8]) -> Result<MuxEnvelope<'_>, serde_json::Error> {
    serde_json::from_slice(line)
}

/// The header of a binary frame, `None` for a JSON line. A frame whose header
/// does not decode is `Err`: it rides control, where the receiver refuses it.
fn frame_header(message: &[u8]) -> Option<Result<FrameHeader, cmux_ipc::BadMessage>> {
    if message.first() != Some(&MAGIC) {
        return None;
    }
    let header = message.first_chunk::<HEADER_LEN>().copied().map_or(
        Err(cmux_ipc::BadMessage::Truncated { have: message.len(), need: HEADER_LEN }),
        |bytes| FrameHeader::decode(bytes, cmux_ipc::MAX_FRAME_PAYLOAD),
    );
    Some(header)
}

/// The lane of a binary frame, from its header alone.
///
/// A call frame (REQUEST, REPLY, ERROR, CANCEL, TYPED_EVENT) carries its
/// method or event id, so a reply rides its request's lane with no request
/// tracking. A refined route takes `RemoteRoute::frame_lane`, since the field
/// it refines on is in the payload. Stream data (output, grid, snapshots,
/// replay, ring) rides bulk like the attach events it replaces, terminal input
/// rides interactive, and an id the schema does not route stays in the
/// interactive sequence, as an unknown command line does.
pub(crate) fn classify_frame(header: &FrameHeader) -> Lane {
    let lanes = RemoteLanes::schema();
    match header.kind() {
        FrameKind::Input => Lane::Interactive,
        FrameKind::Output
        | FrameKind::Grid
        | FrameKind::Event
        | FrameKind::Snapshot
        | FrameKind::SnapshotMore
        | FrameKind::Replay
        | FrameKind::Ring
        | FrameKind::RingAdvance
        | FrameKind::SharedBuffer => Lane::Bulk,
        FrameKind::RingAck => Lane::Control,
        FrameKind::Request
        | FrameKind::Reply
        | FrameKind::Error
        | FrameKind::Cancel
        | FrameKind::TypedEvent => {
            let Some(call) = header.call() else { return Lane::Control };
            let id = call.method_id;
            let family = id.to_be_bytes()[0];
            let route = if (EVENT_FAMILIES.0..=EVENT_FAMILIES.1).contains(&family) {
                lanes.event_id(EventId::new(id))
            } else {
                lanes.command_id(MethodId::new(id))
            };
            route.map_or(Lane::Interactive, |route| lane_of(route.frame_lane()))
        }
    }
}

impl MuxLaneTracker {
    pub(crate) fn observe_request(&self, line: &[u8], lane: Lane) {
        // A frame's reply names its method; only lines need tracking.
        if line.first() == Some(&MAGIC) {
            return;
        }
        let Ok(envelope) = parse_envelope(line) else { return };
        if let Some(id) = envelope.id {
            self.track(id, ResponseDisposition::Forward(lane));
        }
    }

    pub(crate) fn suppress_response(&self, id: u64) {
        self.track(id, ResponseDisposition::Suppress);
    }

    pub(crate) fn classify_server_line(&self, line: &[u8]) -> Option<Lane> {
        if let Some(header) = frame_header(line) {
            return Some(header.map_or(Lane::Control, |header| classify_frame(&header)));
        }
        let Ok(envelope) = parse_envelope(line) else {
            return Some(Lane::Control);
        };
        if let Some(event) = envelope.event.as_ref().map(MuxName::as_str) {
            let route = RemoteLanes::schema().event(event);
            // An event the schema does not know rides control, beside the
            // replies and events it cannot be ordered against.
            return Some(route.map_or(Lane::Control, |route| {
                lane_of(route.lane_for(route.refine.and_then(|r| envelope.field(r.field))))
            }));
        }
        match envelope
            .id
            .and_then(|id| {
                self.state.lock().unwrap_or_else(PoisonError::into_inner).requests.remove(&id)
            })
            .map(|tracked| tracked.disposition)
        {
            Some(ResponseDisposition::Forward(lane)) => Some(lane),
            Some(ResponseDisposition::Suppress) => None,
            None => Some(Lane::Control),
        }
    }

    fn track(&self, id: u64, disposition: ResponseDisposition) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.next_token == u64::MAX {
            state.requests.clear();
            state.order.clear();
            state.next_token = 0;
        }
        state.next_token += 1;
        let token = state.next_token;
        state.requests.insert(id, TrackedResponse { disposition, token });
        state.order.push_back((id, token));

        while state.order.len() > MAX_TRACKED_REQUESTS {
            let Some((old_id, old_token)) = state.order.pop_front() else { break };
            if state.requests.get(&old_id).is_some_and(|tracked| tracked.token == old_token) {
                state.requests.remove(&old_id);
            }
        }
    }
}

/// The lane of a message the local mux client sends.
pub(crate) fn classify_client_line(line: &[u8]) -> Lane {
    if let Some(header) = frame_header(line) {
        return header.map_or(Lane::Control, |header| classify_frame(&header));
    }
    let Ok(envelope) = parse_envelope(line) else { return Lane::Control };
    let Some(name) = envelope.cmd.as_ref().map(MuxName::as_str) else { return Lane::Control };
    match RemoteLanes::schema().command(name) {
        Some(route) => lane_of(route.lane_for(route.refine.and_then(|r| envelope.field(r.field)))),
        // A command this build does not know may mutate: it stays in the
        // interactive sequence, behind input the client sent first.
        None => Lane::Interactive,
    }
}

#[cfg(test)]
#[path = "mux_lanes_tests.rs"]
mod tests;
