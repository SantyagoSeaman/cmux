//! Values shaped by cmux-tui/spec/resource-operations-v2.json, the catalog
//! the resource router validates v2 params against. A generated value fits
//! its catalog type, except where the input asks for a free JSON value
//! instead, so the fuzzer reaches both the accept and the refuse paths of
//! every operation.

// Each catalog target uses part of this.
#![allow(dead_code)]

use std::sync::OnceLock;

use arbitrary::{Result, Unstructured};
use base64::Engine;
use serde_json::{Map, Value, json};

const CATALOG_JSON: &str = include_str!("../../../spec/resource-operations-v2.json");

/// Nesting past this gives a scalar, so one input cannot recurse without end.
const MAX_DEPTH: u32 = 6;

pub fn catalog() -> &'static Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| serde_json::from_str(CATALOG_JSON).unwrap_or(Value::Null))
}

/// Every catalog operation by wire name, in name order.
pub fn operations() -> &'static [(String, Value)] {
    static OPERATIONS: OnceLock<Vec<(String, Value)>> = OnceLock::new();
    OPERATIONS.get_or_init(|| {
        let mut operations = catalog()["operations"]
            .as_object()
            .map(|operations| {
                operations.iter().map(|(name, value)| (name.clone(), value.clone())).collect()
            })
            .unwrap_or_else(Vec::new);
        operations.sort_by(|left: &(String, Value), right| left.0.cmp(&right.0));
        operations
    })
}

/// The public id prefix of a resource or selector name (resource.rs,
/// `PUBLIC_ID_PREFIXES`).
fn id_prefix(resource: &str) -> &str {
    match resource {
        "workspace" => "ws",
        "terminal" => "term",
        "frontend_projection" => "projection",
        "pairing_request" => "pairing",
        other => other,
    }
}

fn hex_id(u: &mut Unstructured<'_>, prefix: &str) -> Result<String> {
    let bytes: [u8; 16] = u.arbitrary()?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{prefix}_{hex}"))
}

/// A selector for the `scope` selector: `current`, `name:<text>`, a public
/// id of the scope, or free text.
pub fn selector(u: &mut Unstructured<'_>, scope: &str) -> Result<Value> {
    Ok(Value::String(match u.int_in_range(0..=3u8)? {
        0 => "current".to_owned(),
        1 => format!("name:{}", u.arbitrary::<String>()?),
        2 => hex_id(u, id_prefix(scope))?,
        _ => u.arbitrary::<String>()?,
    }))
}

/// Any JSON value.
pub fn any_json(u: &mut Unstructured<'_>, depth: u32) -> Result<Value> {
    let top = if depth >= MAX_DEPTH { 4 } else { 6 };
    Ok(match u.int_in_range(0..=top)? {
        0 => Value::Null,
        1 => Value::Bool(u.arbitrary()?),
        2 => json!(u.arbitrary::<i64>()?),
        3 => serde_json::Number::from_f64(u.arbitrary()?).map_or(Value::Null, Value::Number),
        4 => Value::String(u.arbitrary()?),
        5 => {
            let count = u.int_in_range(0..=4usize)?;
            Value::Array((0..count).map(|_| any_json(u, depth + 1)).collect::<Result<_>>()?)
        }
        _ => {
            let count = u.int_in_range(0..=4usize)?;
            let mut map = Map::new();
            for _ in 0..count {
                map.insert(u.arbitrary()?, any_json(u, depth + 1)?);
            }
            Value::Object(map)
        }
    })
}

fn length_bounds(descriptor: &Value) -> (usize, usize) {
    let read = |names: [&str; 2], default: u64| {
        names.iter().find_map(|name| descriptor[*name].as_u64()).unwrap_or(default)
    };
    let minimum = usize::try_from(read(["min_length", "min_items"], 0)).unwrap_or(0);
    let maximum = usize::try_from(read(["max_length", "max_items"], 64)).unwrap_or(64);
    (minimum, maximum.max(minimum))
}

fn string_of_length(u: &mut Unstructured<'_>, descriptor: &Value) -> Result<String> {
    let (minimum, maximum) = length_bounds(descriptor);
    let mut text: String = u.arbitrary()?;
    let mut end = text.len().min(maximum);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    while text.len() < minimum {
        text.push('a');
    }
    Ok(text)
}

fn number_in_range(
    u: &mut Unstructured<'_>,
    descriptor: &Value,
    low: f64,
    high: f64,
) -> Result<f64> {
    let low = descriptor["minimum"].as_f64().unwrap_or(low).max(low);
    let high = descriptor["maximum"].as_f64().unwrap_or(high).min(high);
    let fraction = f64::from(u.arbitrary::<u32>()?) / f64::from(u32::MAX);
    Ok(if high > low { low + (high - low) * fraction } else { low })
}

fn primitive(u: &mut Unstructured<'_>, descriptor: &Value, depth: u32) -> Result<Value> {
    Ok(match descriptor["name"].as_str().unwrap_or_default() {
        "string" => Value::String(string_of_length(u, descriptor)?),
        "base64" => {
            let bytes: Vec<u8> = u.arbitrary()?;
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        "boolean" => Value::Bool(u.arbitrary()?),
        "decimal" => Value::String(u.arbitrary::<u64>()?.to_string()),
        "float64" => serde_json::Number::from_f64(number_in_range(u, descriptor, -1e9, 1e9)?)
            .map_or(Value::Null, Value::Number),
        "uint16" => json!(number_in_range(u, descriptor, 0.0, 65_535.0)?.round() as u64),
        "uint32" => json!(number_in_range(u, descriptor, 0.0, 4_294_967_295.0)?.round() as u64),
        "int32" => {
            json!(number_in_range(u, descriptor, -2_147_483_648.0, 2_147_483_647.0)?.round() as i64)
        }
        _ => any_json(u, depth)?,
    })
}

/// A value of catalog type `descriptor`; `bindings` are the parameters of
/// the generic being applied.
pub fn fit(
    u: &mut Unstructured<'_>,
    descriptor: &Value,
    bindings: &Map<String, Value>,
    depth: u32,
) -> Result<Value> {
    // One value in sixteen, and every value past the depth cap, is free.
    if depth >= MAX_DEPTH || u.ratio(1u8, 16u8)? {
        return any_json(u, depth);
    }
    let next = depth + 1;
    Ok(match descriptor["kind"].as_str().unwrap_or_default() {
        "primitive" => primitive(u, descriptor, depth)?,
        "enum" => match descriptor["values"].as_array() {
            Some(values) if !values.is_empty() => u.choose(values)?.clone(),
            _ => any_json(u, depth)?,
        },
        "array" => {
            let (minimum, maximum) = length_bounds(descriptor);
            let count = u.int_in_range(minimum..=maximum.min(minimum + 4))?;
            let items = &descriptor["items"];
            Value::Array((0..count).map(|_| fit(u, items, bindings, next)).collect::<Result<_>>()?)
        }
        "map" => {
            let mut map = Map::new();
            for _ in 0..u.int_in_range(0..=3usize)? {
                map.insert(u.arbitrary()?, fit(u, &descriptor["values"], bindings, next)?);
            }
            Value::Object(map)
        }
        "nullable" => {
            if u.arbitrary()? {
                Value::Null
            } else {
                fit(u, &descriptor["value"], bindings, next)?
            }
        }
        "object" => object(u, descriptor["fields"].as_object(), bindings, next)?,
        "ref" => {
            let name = descriptor["name"].as_str().unwrap_or_default();
            fit(u, &catalog()["types"][name], bindings, next)?
        }
        "apply" => {
            let name = descriptor["name"].as_str().unwrap_or_default();
            let generic = &catalog()["generics"][name];
            let mut applied = bindings.clone();
            if let (Some(names), Some(arguments)) =
                (generic["parameters"].as_array(), descriptor["arguments"].as_array())
            {
                for (name, argument) in names.iter().zip(arguments) {
                    if let Some(name) = name.as_str() {
                        applied.insert(name.to_owned(), argument.clone());
                    }
                }
            }
            fit(u, &generic["body"], &applied, next)?
        }
        "parameter" => {
            let name = descriptor["name"].as_str().unwrap_or_default();
            match bindings.get(name) {
                Some(target) => fit(u, target, bindings, next)?,
                None => any_json(u, depth)?,
            }
        }
        "selector" => selector(u, "")?,
        "resource_id" => {
            let resource = descriptor["resource"].as_str().unwrap_or_default();
            Value::String(hex_id(u, id_prefix(resource))?)
        }
        "union" => match descriptor["variants"].as_array() {
            Some(variants) if !variants.is_empty() => {
                let variant = u.choose(variants)?;
                fit(u, variant, bindings, next)?
            }
            _ => any_json(u, depth)?,
        },
        _ => any_json(u, depth)?,
    })
}

/// An object of catalog `fields`: every required field, each optional field
/// when the input says so.
pub fn object(
    u: &mut Unstructured<'_>,
    fields: Option<&Map<String, Value>>,
    bindings: &Map<String, Value>,
    depth: u32,
) -> Result<Value> {
    let mut object = Map::new();
    for (name, field) in fields.into_iter().flatten() {
        if field["required"] == Value::Bool(true) || u.arbitrary()? {
            object.insert(name.clone(), fit(u, &field["type"], bindings, depth)?);
        }
    }
    Ok(Value::Object(object))
}
