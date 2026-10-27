//! The lane each command and event rides over a cmux-remote transport,
//! computed from spec/sdk-schema.json (and the framed ids of
//! spec/resource-operations-v2.json) plus the reviewed exceptions in
//! `remote-lanes.json` (p05-ipc task 152). `src/remote_lanes.rs` includes the
//! output; cmux-remote's `mux_lanes` classifies with it. Shared by `build.rs`
//! and the crate's tests, which feed it schemas that must be refused.
//!
//! The vocabulary is the schema's priority (`input`, `control`, `bulk`,
//! `background`); cmux-remote maps it onto its wire lanes. A remote lane is
//! also an ordering domain, so the rules keep together what must stay ordered:
//!
//! 1. A command that opens an attach stream rides bulk with the stream's
//!    events; one that opens a subscription rides control with its events.
//! 2. `input` priority rides input.
//! 3. A read-only command of `bulk` or `background` priority rides bulk: a
//!    large or idle reply never holds input replies behind it.
//! 4. A read-only command the schema lets pass the sequence (`ordering`
//!    `independent`, or a `crosses_stream_ok` reason) rides control.
//! 5. Every other command rides input, the ordered sequence, so it never
//!    passes input or a mutation the client sent first.
//! 6. An event that only attach streams carry rides bulk, in order with the
//!    stream's output; every other event rides its schema priority.
//!
//! An exception names a command, a `prefix-*` pattern or a family, with its
//! reason; it may refine the lane on one envelope field (`op`, `mode`,
//! `scope`). The build refuses an exception that names nothing, repeats the
//! rule's lane, or refines on a field the schema does not declare, so the file
//! never drifts from the schema.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use serde_json::{Map, Value};

/// Where an exception or a schema entry is, and why it was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for LaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

fn error(path: impl Into<String>, message: impl Into<String>) -> LaneError {
    LaneError { path: path.into(), message: message.into() }
}

/// A lane in the schema's priority vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Input,
    Control,
    Bulk,
    Background,
}

impl Lane {
    fn parse(text: &str) -> Option<Lane> {
        match text {
            "input" => Some(Lane::Input),
            "control" => Some(Lane::Control),
            "bulk" => Some(Lane::Bulk),
            "background" => Some(Lane::Background),
            _ => None,
        }
    }

    /// The `PriorityHint` variant.
    pub fn variant(self) -> &'static str {
        match self {
            Lane::Input => "Input",
            Lane::Control => "Control",
            Lane::Bulk => "Bulk",
            Lane::Background => "Background",
        }
    }
}

/// Which rule chose a lane (`RouteRule` in `src/remote_lanes.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    Exception,
    AttachStream,
    SubscribeStream,
    InputPriority,
    BulkRead,
    PassingRead,
    Sequence,
    AttachEvent,
    EventPriority,
    Unlisted,
}

impl Rule {
    fn variant(self) -> &'static str {
        match self {
            Rule::Exception => "Exception",
            Rule::AttachStream => "AttachStream",
            Rule::SubscribeStream => "SubscribeStream",
            Rule::InputPriority => "InputPriority",
            Rule::BulkRead => "BulkRead",
            Rule::PassingRead => "PassingRead",
            Rule::Sequence => "Sequence",
            Rule::AttachEvent => "AttachEvent",
            Rule::EventPriority => "EventPriority",
            Rule::Unlisted => "Unlisted",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Rule::AttachStream => {
                "opens an attach stream: the reply and the stream's events share one ordered lane"
            }
            Rule::SubscribeStream => {
                "opens a subscription: the reply rides control with the subscription's events"
            }
            Rule::InputPriority => "schema priority input",
            Rule::BulkRead => {
                "read-only bulk or background: a large or idle reply never holds input replies"
            }
            Rule::PassingRead => {
                "read-only and the schema lets it pass the sequence (independent or crosses_stream_ok)"
            }
            Rule::Sequence => {
                "stays in the ordered sequence behind input and mutations the client sent first"
            }
            Rule::AttachEvent => {
                "only attach streams carry it: ordered with the stream's output on bulk"
            }
            Rule::EventPriority => "schema priority",
            // Exceptions and unlisted events carry the reason the file gives.
            Rule::Exception | Rule::Unlisted => "",
        }
    }
}

/// A field of the line envelope a lane may be refined on.
pub const REFINE_FIELDS: [(&str, &str); 3] = [("op", "Op"), ("mode", "Mode"), ("scope", "Scope")];

/// `lane` when the envelope's `field` equals `equals`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refine {
    /// `RefineField` variant.
    pub field: &'static str,
    pub equals: String,
    pub lane: Lane,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub lane: Lane,
    pub refine: Option<Refine>,
    pub rule: Rule,
    pub reason: String,
}

impl Route {
    fn rule(lane: Lane, rule: Rule) -> Route {
        Route { lane, refine: None, rule, reason: rule.reason().to_owned() }
    }
}

/// Every route of one schema build.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Routes {
    /// v12 command name -> route.
    pub commands: BTreeMap<String, Route>,
    /// Event name -> route, unlisted events included.
    pub events: BTreeMap<String, Route>,
    /// Framed command or v2 operation id -> route.
    pub command_ids: BTreeMap<u16, Route>,
    /// Event id -> route.
    pub event_ids: BTreeMap<u16, Route>,
}

/// One exception from `remote-lanes.json`.
struct Exception {
    path: String,
    lane: Lane,
    when: Option<(String, String, Lane)>,
    reason: String,
    /// A family or `prefix-*` pattern, not one name.
    group: bool,
    used: bool,
}

/// The exceptions of one section (`commands` or `events`).
struct Exceptions {
    families: BTreeMap<String, Exception>,
    names: BTreeMap<String, Exception>,
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, LaneError> {
    value.as_object().ok_or_else(|| error(path, "must be an object"))
}

fn lane_at(node: &Map<String, Value>, path: &str) -> Result<Lane, LaneError> {
    node.get("lane")
        .and_then(Value::as_str)
        .and_then(Lane::parse)
        .ok_or_else(|| error(format!("{path}.lane"), "must be input, control, bulk or background"))
}

fn exception(path: String, group: bool, value: &Value) -> Result<Exception, LaneError> {
    let node = object(value, &path)?;
    for key in node.keys() {
        if !matches!(key.as_str(), "lane" | "when" | "reason") {
            return Err(error(format!("{path}.{key}"), "is not lane, when or reason"));
        }
    }
    let lane = lane_at(node, &path)?;
    let reason = node
        .get("reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .ok_or_else(|| error(format!("{path}.reason"), "must be a non-empty reason"))?
        .to_owned();
    let when = match node.get("when") {
        None => None,
        Some(value) => {
            let when_path = format!("{path}.when");
            let when = object(value, &when_path)?;
            let field = when
                .get("field")
                .and_then(Value::as_str)
                .ok_or_else(|| error(format!("{when_path}.field"), "must be a string"))?;
            let equals = when
                .get("equals")
                .and_then(Value::as_str)
                .ok_or_else(|| error(format!("{when_path}.equals"), "must be a string"))?;
            let refined = lane_at(when, &when_path)?;
            if refined == lane {
                return Err(error(when_path, "refines to the lane it already has"));
            }
            Some((field.to_owned(), equals.to_owned(), refined))
        }
    };
    Ok(Exception { path, lane, when, reason, group, used: false })
}

fn exceptions(doc: &Value, section: &str) -> Result<Exceptions, LaneError> {
    let mut out = Exceptions { families: BTreeMap::new(), names: BTreeMap::new() };
    let Some(value) = doc.get(section) else {
        return Ok(out);
    };
    let node = object(value, &format!("remote-lanes.json $.{section}"))?;
    for (key, value) in node {
        let path = format!("remote-lanes.json $.{section}.{key}");
        let (target, families) = match key.as_str() {
            "families" => (&mut out.families, true),
            "names" => (&mut out.names, false),
            // Unlisted events are read by `unlisted_events`.
            "unlisted" if section == "events" => continue,
            _ => return Err(error(path, "is not families or names")),
        };
        for (name, entry) in object(value, &path)? {
            let group = families || name.ends_with('*');
            target.insert(name.clone(), exception(format!("{path}.{name}"), group, entry)?);
        }
    }
    Ok(out)
}

impl Exceptions {
    /// The exception for `name` in `family`: its exact name, then the one
    /// `prefix-*` pattern it matches, then its family.
    fn find(&mut self, name: &str, family: &str) -> Result<Option<&mut Exception>, LaneError> {
        if self.names.contains_key(name) {
            return Ok(self.names.get_mut(name));
        }
        let mut patterns = self.names.iter().filter(|(pattern, _)| {
            pattern.strip_suffix('*').is_some_and(|prefix| name.starts_with(prefix))
        });
        let pattern = match (patterns.next(), patterns.next()) {
            (Some((pattern, _)), None) => Some(pattern.clone()),
            (Some((first, _)), Some((second, _))) => {
                return Err(error(name, format!("matches both exceptions {first} and {second}")));
            }
            (None, _) => None,
        };
        Ok(match pattern {
            Some(pattern) => self.names.get_mut(&pattern),
            None => self.families.get_mut(family),
        })
    }

    fn unused(&self) -> Option<&Exception> {
        self.families.values().chain(self.names.values()).find(|exception| !exception.used)
    }
}

/// The field names a schema object type declares.
fn declared_fields(node: Option<&Value>) -> Vec<&str> {
    node.and_then(|n| n.get("fields"))
        .and_then(Value::as_object)
        .map_or_else(Vec::new, |fields| fields.keys().map(String::as_str).collect())
}

/// Applies the exception (if any) on top of the rule's route.
fn apply(
    exceptions: &mut Exceptions,
    name: &str,
    family: &str,
    fields: &[&str],
    rule: Route,
) -> Result<Route, LaneError> {
    let Some(exception) = exceptions.find(name, family)? else {
        return Ok(rule);
    };
    exception.used = true;
    let refine = match &exception.when {
        None => None,
        Some((field, equals, lane)) => {
            let variant = REFINE_FIELDS
                .iter()
                .find(|(wire, _)| wire == field)
                .map(|(_, variant)| *variant)
                .ok_or_else(|| {
                    error(format!("{}.when.field", exception.path), "must be op, mode or scope")
                })?;
            if !fields.contains(&field.as_str()) {
                return Err(error(
                    format!("{}.when.field", exception.path),
                    format!("{name} does not declare {field}"),
                ));
            }
            Some(Refine { field: variant, equals: equals.clone(), lane: *lane })
        }
    };
    // A named exception that changes nothing would hide a later schema change
    // behind a stale entry; a family or pattern may cover members the rule
    // already routes there.
    if refine.is_none() && exception.lane == rule.lane && !exception.group {
        return Err(error(&exception.path, format!("{name} already rides this lane by rule")));
    }
    Ok(Route {
        lane: exception.lane,
        refine,
        rule: Rule::Exception,
        reason: exception.reason.clone(),
    })
}

/// `family id -> name` from the schema's `families` table.
fn family_names(doc: &Value) -> Result<BTreeMap<u64, String>, LaneError> {
    let Some(families) = doc.get("families") else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (name, node) in object(families, "sdk-schema.json $.families")? {
        let id = node.get("id").and_then(Value::as_u64).ok_or_else(|| {
            error(format!("sdk-schema.json $.families.{name}.id"), "must be an unsigned integer")
        })?;
        out.insert(id, name.clone());
    }
    Ok(out)
}

fn priority(node: &Value, path: &str) -> Result<Lane, LaneError> {
    node.get("priority").and_then(Value::as_str).and_then(Lane::parse).ok_or_else(|| {
        error(format!("{path}.priority"), "a framed entry needs input, control, bulk or background")
    })
}

/// The id of a framed entry, or `None` when it has none (not framed).
fn wire_id(node: &Value, key: &str, path: &str) -> Result<Option<u16>, LaneError> {
    match node.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|id| u16::try_from(id).ok())
            .map(Some)
            .ok_or_else(|| error(format!("{path}.{key}"), "must be a u16")),
    }
}

/// Rules 1-5 for a command or v2 operation.
fn command_rule(node: &Value, path: &str) -> Result<Route, LaneError> {
    match node.get("stream").and_then(|s| s.get("kind")).and_then(Value::as_str) {
        Some("attach") => return Ok(Route::rule(Lane::Bulk, Rule::AttachStream)),
        Some("subscribe") => return Ok(Route::rule(Lane::Control, Rule::SubscribeStream)),
        _ => {}
    }
    let lane = priority(node, path)?;
    let read_only = node.get("durability").and_then(Value::as_str) == Some("read_only");
    let passes = node.get("ordering").and_then(Value::as_str) == Some("independent")
        || node.get("crosses_stream_ok").is_some();
    Ok(match lane {
        Lane::Input => Route::rule(Lane::Input, Rule::InputPriority),
        Lane::Bulk | Lane::Background if read_only => Route::rule(Lane::Bulk, Rule::BulkRead),
        Lane::Control if read_only && passes => Route::rule(Lane::Control, Rule::PassingRead),
        _ => Route::rule(Lane::Input, Rule::Sequence),
    })
}

/// Rule 6 for an event.
fn event_rule(node: &Value, path: &str) -> Result<Route, LaneError> {
    let streams: Vec<&str> = node
        .get("streams")
        .and_then(Value::as_array)
        .map(|streams| streams.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !streams.is_empty() && streams.iter().all(|stream| stream.starts_with("attach-")) {
        return Ok(Route::rule(Lane::Bulk, Rule::AttachEvent));
    }
    Ok(Route::rule(priority(node, path)?, Rule::EventPriority))
}

fn family_of(id: u16, families: &BTreeMap<u64, String>, path: &str) -> Result<String, LaneError> {
    families
        .get(&u64::from(id >> 8))
        .cloned()
        .ok_or_else(|| error(path, format!("family 0x{:02X} is not declared", id >> 8)))
}

/// Routes every framed v12 command and event of `sdk`, every framed v2
/// operation of `v2`, and the unlisted events, under the exceptions of
/// `lanes` (the `remote-lanes.json` document).
pub fn read(sdk: &Value, v2: &Value, lanes: &Value) -> Result<Routes, LaneError> {
    for key in object(lanes, "remote-lanes.json $")?.keys() {
        if !matches!(key.as_str(), "description" | "commands" | "events") {
            return Err(error(
                format!("remote-lanes.json $.{key}"),
                "is not description, commands or events",
            ));
        }
    }
    let families = family_names(sdk)?;
    let mut routes = Routes::default();

    let mut commands = exceptions(lanes, "commands")?;
    if let Some(entries) = sdk.get("commands") {
        for (name, node) in object(entries, "sdk-schema.json $.commands")? {
            let path = format!("sdk-schema.json $.commands.{name}");
            let Some(id) = wire_id(node, "method_id", &path)? else {
                continue;
            };
            let family = family_of(id, &families, &path)?;
            let fields = declared_fields(node.get("request"));
            let route = apply(&mut commands, name, &family, &fields, command_rule(node, &path)?)?;
            routes.command_ids.insert(id, route.clone());
            routes.commands.insert(name.clone(), route);
        }
    }
    if let Some(unused) = commands.unused() {
        return Err(error(&unused.path, "names no framed command or family"));
    }

    if let Some(entries) = v2.get("operations") {
        for (name, node) in object(entries, "resource-operations-v2.json $.operations")? {
            let path = format!("resource-operations-v2.json $.operations.{name}");
            if let Some(id) = wire_id(node, "method_id", &path)? {
                routes.command_ids.insert(id, command_rule(node, &path)?);
            }
        }
    }

    let mut events = exceptions(lanes, "events")?;
    if let Some(entries) = sdk.get("events") {
        for (name, node) in object(entries, "sdk-schema.json $.events")? {
            let path = format!("sdk-schema.json $.events.{name}");
            let Some(id) = wire_id(node, "event_id", &path)? else {
                continue;
            };
            let family = family_of(id, &families, &path)?;
            let fields = declared_fields(node.get("payload"));
            let route = apply(&mut events, name, &family, &fields, event_rule(node, &path)?)?;
            routes.event_ids.insert(id, route.clone());
            routes.events.insert(name.clone(), route);
        }
    }
    if let Some(unused) = events.unused() {
        return Err(error(&unused.path, "names no framed event or family"));
    }
    unlisted_events(lanes, sdk, &mut routes)?;
    Ok(routes)
}

/// Events a daemon emits that the schema does not declare yet. Each must
/// leave this list when the schema declares it.
fn unlisted_events(lanes: &Value, sdk: &Value, routes: &mut Routes) -> Result<(), LaneError> {
    let Some(unlisted) = lanes.get("events").and_then(|events| events.get("unlisted")) else {
        return Ok(());
    };
    for (name, value) in object(unlisted, "remote-lanes.json $.events.unlisted")? {
        let path = format!("remote-lanes.json $.events.unlisted.{name}");
        if sdk.get("events").and_then(|events| events.get(name)).is_some() {
            return Err(error(path, "the schema declares this event; route it there"));
        }
        let exception = exception(path.clone(), false, value)?;
        if exception.when.is_some() {
            return Err(error(format!("{path}.when"), "an unlisted event declares no fields"));
        }
        routes.events.insert(
            name.clone(),
            Route {
                lane: exception.lane,
                refine: None,
                rule: Rule::Unlisted,
                reason: exception.reason,
            },
        );
    }
    Ok(())
}

// Emitting ------------------------------------------------------------------

fn emit_route(route: &Route) -> String {
    let mut out = format!("RemoteRoute {{ lane: PriorityHint::{}, refine: ", route.lane.variant());
    match &route.refine {
        None => out.push_str("None"),
        Some(refine) => {
            let _ = write!(
                out,
                "Some(Refine {{ field: RefineField::{}, equals: {:?}, lane: PriorityHint::{} }})",
                refine.field,
                refine.equals,
                refine.lane.variant()
            );
        }
    }
    let _ =
        write!(out, ", rule: RouteRule::{}, reason: {:?} }}", route.rule.variant(), route.reason);
    out
}

fn emit_names(out: &mut String, routes: &BTreeMap<String, Route>) {
    let mut map = phf_codegen::Map::<&str>::new();
    map.phf_path("::phf");
    for (name, route) in routes {
        map.entry(name, &emit_route(route));
    }
    let _ = write!(out, "{}", map.build());
}

fn emit_ids(out: &mut String, routes: &BTreeMap<u16, Route>) {
    let mut map = phf_codegen::Map::<u16>::new();
    map.phf_path("::phf");
    for (id, route) in routes {
        map.entry(*id, &emit_route(route));
    }
    let _ = write!(out, "{}", map.build());
}

/// Rust source of `static <name>: RemoteLanes`, for `remote_lanes.rs` to include.
pub fn emit(routes: &Routes, name: &str) -> String {
    let mut out = format!("pub(crate) static {name}: RemoteLanes = RemoteLanes {{\ncommands: ");
    emit_names(&mut out, &routes.commands);
    out.push_str(",\nevents: ");
    emit_names(&mut out, &routes.events);
    out.push_str(",\ncommand_ids: ");
    emit_ids(&mut out, &routes.command_ids);
    out.push_str(",\nevent_ids: ");
    emit_ids(&mut out, &routes.event_ids);
    out.push_str(",\n};\n");
    out
}
