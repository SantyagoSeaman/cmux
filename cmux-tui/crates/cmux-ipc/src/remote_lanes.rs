//! The lane each command and event rides over a cmux-remote transport
//! (p05-ipc task 152), in the schema's priority vocabulary. build.rs computes
//! it from the schema and the reviewed exceptions in `remote-lanes.json`
//! (build-support/remote_lanes.rs states the rules); nothing generated is
//! committed. cmux-remote maps each `PriorityHint` onto a wire lane.
//!
//! Like `MethodTables`, a lookup is one perfect-hash probe and never
//! allocates. A frame is routed by its method or event id from the header, so
//! it is never parsed (design 3.2, "Routing never parses the payload").

use crate::PriorityHint;
use crate::methods::{EventId, MethodId};

/// A line envelope field a route may be refined on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefineField {
    Op,
    Mode,
    Scope,
}

impl RefineField {
    /// The envelope key.
    pub const fn name(self) -> &'static str {
        match self {
            RefineField::Op => "op",
            RefineField::Mode => "mode",
            RefineField::Scope => "scope",
        }
    }
}

/// `lane` when the envelope's `field` equals `equals`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Refine {
    pub field: RefineField,
    pub equals: &'static str,
    pub lane: PriorityHint,
}

/// Which rule of build-support/remote_lanes.rs chose a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RouteRule {
    /// A reviewed exception in `remote-lanes.json`.
    Exception,
    /// Opens an attach stream: rides bulk with the stream's events.
    AttachStream,
    /// Opens a subscription: rides control with its events.
    SubscribeStream,
    /// Schema priority `input`.
    InputPriority,
    /// Read-only of bulk or background priority: rides bulk.
    BulkRead,
    /// Read-only and allowed to pass the sequence: rides control.
    PassingRead,
    /// Everything else: rides input, ordered behind what the client sent first.
    Sequence,
    /// Carried by a subscription: control, in order with its other events.
    SubscribeEvent,
    /// Carried only by attach streams: bulk, with the stream's output.
    AttachEvent,
    /// An event's schema priority.
    EventPriority,
    /// An event the schema does not declare yet (`events.unlisted`).
    Unlisted,
}

/// Where one command or event rides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RemoteRoute {
    pub lane: PriorityHint,
    pub refine: Option<Refine>,
    pub rule: RouteRule,
    /// The rule's or the exception's reason.
    pub reason: &'static str,
}

impl RemoteRoute {
    /// The lane of a line whose refining field holds `value` (`None` when
    /// the line has no such field).
    pub fn lane_for(&self, value: Option<&str>) -> PriorityHint {
        match self.refine {
            Some(refine) if value == Some(refine.equals) => refine.lane,
            _ => self.lane,
        }
    }

    /// The lane of a frame, whose payload is never read: input when either
    /// lane of a refined route is input, so the frame cannot pass input the
    /// line form would have stayed behind; the base lane otherwise.
    pub fn frame_lane(&self) -> PriorityHint {
        match self.refine {
            Some(refine) if refine.lane == PriorityHint::Input => PriorityHint::Input,
            _ => self.lane,
        }
    }
}

/// Every route of one schema build.
#[derive(Debug)]
pub struct RemoteLanes {
    commands: phf::Map<&'static str, RemoteRoute>,
    events: phf::Map<&'static str, RemoteRoute>,
    command_ids: phf::Map<u16, RemoteRoute>,
    event_ids: phf::Map<u16, RemoteRoute>,
}

include!(concat!(env!("OUT_DIR"), "/remote_lanes.rs"));

impl RemoteLanes {
    /// The routes of the schema this binary was built from.
    pub fn schema() -> &'static RemoteLanes {
        &REMOTE_LANES
    }

    /// The route of the v12 command named `name` (a line's `cmd`).
    pub fn command(&self, name: &str) -> Option<&RemoteRoute> {
        self.commands.get(name)
    }

    /// The route of the event named `name` (a line's `event`).
    pub fn event(&self, name: &str) -> Option<&RemoteRoute> {
        self.events.get(name)
    }

    /// The route of a framed command or v2 operation.
    pub fn command_id(&self, id: MethodId) -> Option<&RemoteRoute> {
        self.command_ids.get(&id.raw())
    }

    /// The route of a framed event.
    pub fn event_id(&self, id: EventId) -> Option<&RemoteRoute> {
        self.event_ids.get(&id.raw())
    }

    /// Every v12 command and its route.
    pub fn commands(&self) -> impl Iterator<Item = (&'static str, &RemoteRoute)> + '_ {
        self.commands.entries().map(|(name, route)| (*name, route))
    }

    /// Every event and its route, unlisted events included.
    pub fn events(&self) -> impl Iterator<Item = (&'static str, &RemoteRoute)> + '_ {
        self.events.entries().map(|(name, route)| (*name, route))
    }
}
