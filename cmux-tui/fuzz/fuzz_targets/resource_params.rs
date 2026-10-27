//! v2 request validation against the operation catalog
//! (resource_router.rs `parse_resource_line` then
//! `validate_resource_envelope`, catalog_validation.rs): what every
//! `cmux.protocol/2` line from the CLI, MCP, an SDK or a peer passes before
//! dispatch. Chromium's Mojo rule applies (mojo/public/cpp/bindings/lib/
//! validation_errors.h): a bad message is refused with a typed error, never
//! a crash of the receiver.
//!
//! An input whose byte 0 is 0x00 is a line as written (the corpus seeds:
//! the oracle's daemon-ops requests). Any other input builds the line from
//! the catalog: an operation, its selectors and its fields with values that
//! fit their types, except where the input asks for a free member, a free
//! value or a broken envelope member.
//!
//! Properties, beyond "no crash, leak or hang":
//! - a refusal is the request's fault: it never blames the embedded catalog;
//! - an accepted request has every field the catalog requires;
//! - the accepted envelope, sent again as the router re-serializes it, is
//!   accepted again with the same selectors and fields.

#![no_main]

#[path = "support/catalog.rs"]
mod catalog;

use arbitrary::{Result, Unstructured};
use cmux_tui_core::fuzz_entry::{AcceptedResourceRequest, resource_request};
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value, json};

fn params(u: &mut Unstructured<'_>, descriptor: &Value) -> Result<Value> {
    let mut params = Map::new();
    for (name, requiredness) in descriptor["selectors"].as_object().into_iter().flatten() {
        if requiredness == "required" && !u.ratio(1u8, 32u8)? || u.arbitrary()? {
            params.insert(name.clone(), catalog::selector(u, name)?);
        }
    }
    if let Value::Object(fields) =
        catalog::object(u, descriptor["fields"].as_object(), &Map::new(), 0)?
    {
        params.extend(fields);
    }
    // A free member: an unknown name, or a known one with a free value.
    if u.ratio(1u8, 8u8)? {
        params.insert(u.arbitrary()?, catalog::any_json(u, 0)?);
    }
    Ok(Value::Object(params))
}

fn line(u: &mut Unstructured<'_>) -> Result<String> {
    let operations = catalog::operations();
    if operations.is_empty() {
        return Err(arbitrary::Error::EmptyChoose);
    }
    let (name, operation) = u.choose(operations)?;
    let mut envelope = json!({
        "protocol": "cmux.protocol/2",
        "type": "request",
        "id": format!("fuzz-{}", u.arbitrary::<u16>()?),
        "operation": name,
        "params": params(u, &operation["params"])?,
    });
    let mutation = operation["class"] == "mutation";
    if mutation != u.ratio(1u8, 32u8)? {
        envelope["idempotency_key"] = Value::String(if u.ratio(1u8, 8u8)? {
            u.arbitrary()?
        } else {
            format!("key-{}", u.arbitrary::<u32>()?)
        });
    }
    // A broken envelope member, one input in sixteen.
    if u.ratio(1u8, 16u8)? {
        let member = *u.choose(&["protocol", "type", "id", "operation", "params", "extra"])?;
        envelope[member] = catalog::any_json(u, 0)?;
    }
    Ok(envelope.to_string())
}

fn required_fields_present(accepted: &AcceptedResourceRequest, operation: &str) {
    let Some((_, descriptor)) = catalog::operations().iter().find(|(name, _)| name == operation)
    else {
        panic!("an accepted operation {operation} is not in the catalog");
    };
    for (name, field) in descriptor["params"]["fields"].as_object().into_iter().flatten() {
        if field["required"] == Value::Bool(true) {
            assert!(
                accepted.fields.contains_key(name),
                "{operation} accepted without its required field {name}"
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let message = match data.split_first() {
        Some((0, written)) => match std::str::from_utf8(written) {
            Ok(written) => written.to_owned(),
            Err(_) => return,
        },
        _ => match line(&mut Unstructured::new(data)) {
            Ok(built) => built,
            Err(_) => return,
        },
    };
    let Some(outcome) = resource_request(&message) else {
        assert!(data.first() == Some(&0), "a built line is not routed as v2: {message}");
        return;
    };
    let accepted = match outcome {
        Ok(accepted) => accepted,
        Err(error) => {
            assert!(
                !(error.code == "operation.failed" && error.message.contains("catalog")),
                "the router blames its own catalog ({}: {}) for {message}",
                error.code,
                error.message
            );
            return;
        }
    };
    let operation = accepted.envelope.operation.wire_name();
    required_fields_present(&accepted, operation);

    let resent = serde_json::to_string(&accepted.envelope).expect("an accepted envelope encodes");
    match resource_request(&resent) {
        Some(Ok(again)) => {
            assert_eq!(again.selectors, accepted.selectors, "selectors change on resend: {resent}");
            assert_eq!(again.fields, accepted.fields, "fields change on resend: {resent}");
        }
        Some(Err(error)) => {
            panic!("a re-sent accepted request is refused ({}): {resent}", error.message)
        }
        None => panic!("a re-sent accepted request is not a v2 line: {resent}"),
    }
});
