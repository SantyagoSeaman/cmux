//! Wire names of resource operations round-trip through serde.

use super::ResourceOperation;

#[test]
fn wire_name_round_trips_through_serde() {
    for name in [
        "machine.list",
        "session.journal.append",
        "workspace.create",
        "terminal.output_read",
        "browser.close",
        "git.branches",
        "git.diff",
        "git.files.list",
        "git.files.search",
        "stream.cancel",
        "origin.confirmation.issue",
    ] {
        let operation: ResourceOperation =
            serde_json::from_str(&format!("\"{name}\"")).expect("known operation");
        assert_eq!(operation.wire_name(), name);
        assert_eq!(serde_json::to_string(&operation).unwrap(), format!("\"{name}\""));
    }
}

/// `ALL` lists every operation serde knows, once (the request watchdog
/// names an operation from it).
#[test]
fn all_lists_every_operation_once() {
    let error = serde_json::from_str::<ResourceOperation>("\"no.such.operation\"")
        .expect_err("an unknown operation is refused")
        .to_string();
    let (_, expected) = error.split_once("expected one of ").expect("serde lists its variants");
    let expected = expected.split_once(" at line").map_or(expected, |(names, _)| names);
    let serde: std::collections::BTreeSet<&str> =
        expected.split(", ").map(|name| name.trim_matches('`')).collect();
    let listed: std::collections::BTreeSet<&str> =
        ResourceOperation::ALL.iter().map(|operation| operation.wire_name()).collect();
    assert_eq!(listed.len(), ResourceOperation::ALL.len(), "an operation is listed twice");
    let unlisted: Vec<_> = serde.difference(&listed).collect();
    let unknown: Vec<_> = listed.difference(&serde).collect();
    assert!(
        unlisted.is_empty() && unknown.is_empty(),
        "unlisted {unlisted:?}, unknown {unknown:?}"
    );
}

/// A request's `operation` and `type` are JSON strings only. serde's derived
/// enum decoder also takes the map form `{"session.ping":null}`, which the
/// request head does not route on (server/request_head.rs reads strings), so
/// the head and the envelope named different operations (found by the
/// cmux-tui/fuzz `control_request` target, p15-oracle task 220).
#[test]
fn request_envelope_refuses_the_map_form_of_an_enum_member() {
    let line = |operation: &str, kind: &str| {
        format!(
            r#"{{"protocol":"cmux.protocol/2","type":{kind},"id":"r-1","operation":{operation},"params":{{}}}}"#
        )
    };
    let parse = |line: String| serde_json::from_str::<super::RequestEnvelope>(&line);
    let accepted = parse(line(r#""session.ping""#, r#""request""#)).expect("the string form");
    assert_eq!(accepted.operation, ResourceOperation::SessionPing);
    assert!(parse(line(r#"{"session.ping":null}"#, r#""request""#)).is_err(), "map-form operation");
    assert!(parse(line(r#""session.ping""#, r#"{"request":null}"#)).is_err(), "map-form type");
}
