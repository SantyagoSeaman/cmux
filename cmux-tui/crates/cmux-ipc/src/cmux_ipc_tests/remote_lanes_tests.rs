//! The remote lane routes (build-support/remote_lanes.rs): the generated
//! table agrees with the reader entry by entry, covers every framed method and
//! event, and the reader refuses exceptions that drift from the schema.
//! cmux-remote's mux_lanes tests compare the routes with the hand-written
//! table they replaced.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::PriorityHint;
use crate::methods::MethodTables;
use crate::remote_lanes::{Refine, RefineField, RemoteLanes, RemoteRoute, RouteRule};

#[allow(dead_code, reason = "the build script uses the emitter; the tests use the reader")]
#[path = "../../build-support/remote_lanes.rs"]
mod reader;

fn load(path: PathBuf) -> Value {
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn real_routes() -> reader::Routes {
    reader::read(
        &load(manifest().join("../../spec/sdk-schema.json")),
        &load(manifest().join("../../spec/resource-operations-v2.json")),
        &load(manifest().join("remote-lanes.json")),
    )
    .unwrap()
}

fn hint(lane: reader::Lane) -> PriorityHint {
    match lane {
        reader::Lane::Input => PriorityHint::Input,
        reader::Lane::Control => PriorityHint::Control,
        reader::Lane::Bulk => PriorityHint::Bulk,
        reader::Lane::Background => PriorityHint::Background,
    }
}

fn rule(rule: reader::Rule) -> RouteRule {
    match rule {
        reader::Rule::Exception => RouteRule::Exception,
        reader::Rule::AttachStream => RouteRule::AttachStream,
        reader::Rule::SubscribeStream => RouteRule::SubscribeStream,
        reader::Rule::InputPriority => RouteRule::InputPriority,
        reader::Rule::BulkRead => RouteRule::BulkRead,
        reader::Rule::PassingRead => RouteRule::PassingRead,
        reader::Rule::Sequence => RouteRule::Sequence,
        reader::Rule::AttachEvent => RouteRule::AttachEvent,
        reader::Rule::EventPriority => RouteRule::EventPriority,
        reader::Rule::Unlisted => RouteRule::Unlisted,
    }
}

fn check(found: Option<&RemoteRoute>, expected: &reader::Route, what: &str) {
    let found = found.unwrap_or_else(|| panic!("{what} has no generated route"));
    assert_eq!(found.lane, hint(expected.lane), "{what} lane");
    assert_eq!(found.rule, rule(expected.rule), "{what} rule");
    assert_eq!(found.reason, expected.reason, "{what} reason");
    assert!(!found.reason.is_empty(), "{what} has no reason");
    let refine = expected.refine.as_ref().map(|refine| {
        let field = match refine.field {
            "Op" => RefineField::Op,
            "Mode" => RefineField::Mode,
            "Scope" => RefineField::Scope,
            other => panic!("{what} refines on {other}"),
        };
        (field, refine.equals.as_str(), hint(refine.lane))
    });
    assert_eq!(found.refine.map(|r| (r.field, r.equals, r.lane)), refine, "{what} refine");
}

#[test]
fn remote_lanes_generated_table_matches_the_reader() {
    let routes = real_routes();
    let lanes = RemoteLanes::schema();
    for (name, route) in &routes.commands {
        check(lanes.command(name), route, &format!("command {name}"));
    }
    for (name, route) in &routes.events {
        check(lanes.event(name), route, &format!("event {name}"));
    }
    assert_eq!(lanes.commands().count(), routes.commands.len());
    assert_eq!(lanes.events().count(), routes.events.len());
}

#[test]
fn remote_lanes_route_every_framed_method_and_event_by_name_and_id() {
    let tables = MethodTables::schema();
    let lanes = RemoteLanes::schema();
    for method in tables.v12_families().flat_map(|family| family.iter()) {
        let by_name = lanes.command(method.name).unwrap_or_else(|| panic!("{}", method.name));
        assert_eq!(lanes.command_id(method.id), Some(by_name), "{}", method.name);
    }
    for method in tables.v2_families().flat_map(|family| family.iter()) {
        assert!(lanes.command_id(method.id).is_some(), "{}", method.name);
    }
    for event in tables.event_families().flat_map(|family| family.iter()) {
        let by_name = lanes.event(event.name).unwrap_or_else(|| panic!("{}", event.name));
        assert_eq!(lanes.event_id(event.id), Some(by_name), "{}", event.name);
    }
}

#[test]
fn remote_lanes_refined_routes_pick_by_field_and_frames_stay_behind_input() {
    let lanes = RemoteLanes::schema();
    let paste = lanes.command("paste-image").unwrap();
    assert_eq!(paste.lane_for(Some("commit")), PriorityHint::Input);
    assert_eq!(paste.lane_for(Some("chunk")), PriorityHint::Bulk);
    assert_eq!(paste.lane_for(None), PriorityHint::Bulk);
    // A framed commit cannot be told from a chunk: the paste stays with input.
    assert_eq!(paste.frame_lane(), PriorityHint::Input);

    let copy = lanes.command("copy").unwrap();
    assert_eq!(copy.lane_for(Some("scrollback")), PriorityHint::Bulk);
    assert_eq!(copy.lane_for(Some("screen")), PriorityHint::Input);
    assert_eq!(copy.frame_lane(), PriorityHint::Input);

    let overflow = lanes.event("overflow").unwrap();
    assert_eq!(overflow.lane_for(Some("surface")), PriorityHint::Bulk);
    assert_eq!(overflow.lane_for(None), PriorityHint::Control);
    assert_eq!(overflow.frame_lane(), PriorityHint::Control);

    let plain = RemoteRoute {
        lane: PriorityHint::Bulk,
        refine: None,
        rule: RouteRule::BulkRead,
        reason: "",
    };
    assert_eq!(plain.frame_lane(), PriorityHint::Bulk);
    let refined = RemoteRoute {
        refine: Some(Refine { field: RefineField::Mode, equals: "x", lane: PriorityHint::Control }),
        ..plain
    };
    assert_eq!(refined.frame_lane(), PriorityHint::Bulk);
}

/// A schema with one family of commands and one of events.
fn schema() -> Value {
    json!({
        "families": {"probe": {"id": 9}, "probe-events": {"id": 0xC9}},
        "commands": {
            "probe-write": {"method_id": 0x0900, "priority": "control", "durability": "memory",
                "request": {"kind": "object", "fields": {"op": {}}}},
            "probe-read": {"method_id": 0x0901, "priority": "bulk", "durability": "read_only"},
            "probe-key": {"method_id": 0x0902, "priority": "input", "durability": "memory"},
            "probe-attach": {"method_id": 0x0903, "priority": "control", "durability": "memory",
                "stream": {"kind": "attach"}},
            "probe-subscribe": {"method_id": 0x0904, "priority": "control", "durability": "memory",
                "stream": {"kind": "subscribe"}},
            "unframed": {"priority": "control"}
        },
        "events": {
            "probe-output": {"event_id": 0xC900, "priority": "input", "streams": ["attach-byte"]},
            "probe-changed": {"event_id": 0xC901, "priority": "background",
                "streams": ["subscribe", "attach-byte"]}
        }
    })
}

fn routes(lanes: Value) -> Result<reader::Routes, reader::LaneError> {
    reader::read(&schema(), &json!({}), &lanes)
}

#[test]
fn remote_lanes_rules_route_by_stream_priority_and_durability() {
    let routes = routes(json!({})).unwrap();
    let lane = |name: &str| (routes.commands[name].lane, routes.commands[name].rule);
    use reader::{Lane, Rule};
    assert_eq!(lane("probe-write"), (Lane::Input, Rule::Sequence));
    assert_eq!(lane("probe-read"), (Lane::Bulk, Rule::BulkRead));
    assert_eq!(lane("probe-key"), (Lane::Input, Rule::InputPriority));
    assert_eq!(lane("probe-attach"), (Lane::Bulk, Rule::AttachStream));
    assert_eq!(lane("probe-subscribe"), (Lane::Control, Rule::SubscribeStream));
    assert!(!routes.commands.contains_key("unframed"));
    assert_eq!(routes.command_ids.len(), 5);
    let event = |name: &str| (routes.events[name].lane, routes.events[name].rule);
    assert_eq!(event("probe-output"), (Lane::Bulk, Rule::AttachEvent));
    assert_eq!(event("probe-changed"), (Lane::Background, Rule::EventPriority));
}

#[test]
fn remote_lanes_exceptions_apply_by_name_pattern_and_family() {
    let routes = routes(json!({
        "commands": {
            "families": {"probe": {"lane": "control", "reason": "family"}},
            "names": {
                "probe-k*": {"lane": "bulk", "reason": "pattern"},
                "probe-write": {"lane": "bulk", "reason": "name",
                    "when": {"field": "op", "equals": "commit", "lane": "input"}}
            }
        },
        "events": {"unlisted": {"probe-extra": {"lane": "bulk", "reason": "not declared yet"}}}
    }))
    .unwrap();
    let write = &routes.commands["probe-write"];
    assert_eq!((write.lane, write.reason.as_str()), (reader::Lane::Bulk, "name"));
    assert_eq!(
        write.refine,
        Some(reader::Refine { field: "Op", equals: "commit".into(), lane: reader::Lane::Input })
    );
    assert_eq!(routes.commands["probe-key"].reason, "pattern");
    assert_eq!(routes.commands["probe-read"].reason, "family");
    assert_eq!(routes.commands["probe-read"].lane, reader::Lane::Control);
    assert_eq!(routes.events["probe-extra"].rule, reader::Rule::Unlisted);
}

#[test]
fn remote_lanes_refuse_exceptions_that_drift_from_the_schema() {
    let refused = |lanes: Value, path: &str, message: &str| {
        let error = routes(lanes.clone()).unwrap_err();
        assert!(error.path.ends_with(path), "{lanes}: {error}");
        assert!(error.message.contains(message), "{lanes}: {error}");
    };
    let named = |name: &str, entry: Value| json!({"commands": {"names": {name: entry}}});
    refused(named("gone", json!({"lane": "bulk", "reason": "r"})), ".gone", "names no framed");
    refused(
        json!({"commands": {"families": {"gone": {"lane": "bulk", "reason": "r"}}}}),
        ".gone",
        "names no framed",
    );
    refused(named("probe-read", json!({"lane": "bulk", "reason": "r"})), ".probe-read", "by rule");
    refused(named("probe-read", json!({"lane": "slow", "reason": "r"})), ".lane", "must be");
    refused(named("probe-read", json!({"lane": "control"})), ".reason", "non-empty");
    refused(
        named("probe-read", json!({"lane": "control", "reason": "r", "priority": 1})),
        ".priority",
        "is not lane",
    );
    refused(
        named(
            "probe-read",
            json!({"lane": "control", "reason": "r",
            "when": {"field": "op", "equals": "x", "lane": "input"}}),
        ),
        ".when.field",
        "does not declare op",
    );
    refused(
        named(
            "probe-write",
            json!({"lane": "bulk", "reason": "r",
            "when": {"field": "surface", "equals": "x", "lane": "input"}}),
        ),
        ".when.field",
        "must be op, mode or scope",
    );
    refused(
        named(
            "probe-write",
            json!({"lane": "bulk", "reason": "r",
            "when": {"field": "op", "equals": "x", "lane": "bulk"}}),
        ),
        ".when",
        "already has",
    );
    refused(
        json!({"commands": {"names": {
            "probe-*": {"lane": "bulk", "reason": "r"},
            "probe-w*": {"lane": "control", "reason": "r"}
        }}}),
        "probe-write",
        "matches both",
    );
    refused(
        json!({"events": {"unlisted": {"probe-output": {"lane": "bulk", "reason": "r"}}}}),
        ".probe-output",
        "declares this event",
    );
    refused(json!({"lanes": {}}), ".lanes", "is not description");
}
