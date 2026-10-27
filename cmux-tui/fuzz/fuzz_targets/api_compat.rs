//! The API revision translator (cmux-api-compat): the streaming editor that
//! rewrites every request, response and event between a client's API
//! revision and the daemon's (server/api_revision_wire.rs), and the loader
//! of the compat file it runs (spec/api-compat.json).
//!
//! Two inputs:
//! - a chain of change rules, built the way the crate's property test
//!   builds one (tests/api_compat.rs, `chain`: renames and moves target a
//!   fresh name), a document over the rules' keys and a revision pair. The
//!   streaming rewrite must equal the rules applied one by one to the parsed
//!   document (`Program::apply_reference`), compact or pretty-printed; the
//!   input's spare bytes are rewritten too, and when they are JSON the
//!   rewrite must succeed and give JSON;
//! - a compat file as bytes, loaded, compiled for each message kind in both
//!   directions and run on a message: errors are fine, a panic is not. An
//!   input whose byte 0 is 0x00 is this one: the file, a 0x00 byte, the
//!   message (the corpus seeds: spec/api-compat.json and its fixtures).

#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use cmux_api_compat::{Change, Compat, Conversion, MessageChange, Schema};
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value, json};

const KEYS: [&str; 5] = ["a", "b", "c", "d", "e"];
const MAX_RULES: usize = 24;
const MAX_DEPTH: u32 = 8;

#[derive(Arbitrary, Debug)]
enum Doc {
    Null,
    Bool(bool),
    Int(i8),
    Text(u8),
    Array(Vec<Doc>),
    Object(Vec<(u8, Doc)>),
}

impl Doc {
    fn value(&self, depth: u32) -> Value {
        if depth > MAX_DEPTH {
            return Value::Null;
        }
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Int(value) => json!(i64::from(*value) % 50),
            Self::Text(seed) => match seed % 4 {
                0 => json!(""),
                1 => json!("12"),
                2 => json!(KEYS[usize::from(*seed) % KEYS.len()]),
                _ => json!(format!("~v{}", seed % 8)),
            },
            Self::Array(items) => {
                Value::Array(items.iter().take(4).map(|item| item.value(depth + 1)).collect())
            }
            Self::Object(members) => {
                let mut map = Map::new();
                for (key, member) in members.iter().take(5) {
                    map.insert(
                        KEYS[usize::from(*key) % KEYS.len()].to_owned(),
                        member.value(depth + 1),
                    );
                }
                Value::Object(map)
            }
        }
    }
}

#[derive(Arbitrary, Debug)]
struct Chain {
    rules: Vec<(u32, [u16; 10])>,
    doc: Doc,
    from: u8,
    to: u8,
    spare: Vec<u8>,
}

/// A path token: a key, a wildcard, or a name an earlier rule made.
fn token(seed: u32, fresh_so_far: u32) -> String {
    match seed % 8 {
        0 | 1 => "*".to_owned(),
        2 if fresh_so_far > 0 => format!("r{}", seed % fresh_so_far),
        _ => KEYS[(seed as usize / 8) % KEYS.len()].to_owned(),
    }
}

fn path(seeds: &[u32], fresh_so_far: u32, last_not_wild: bool) -> String {
    let mut out = String::new();
    for (index, seed) in seeds.iter().enumerate() {
        let mut token = token(*seed, fresh_so_far);
        if last_not_wild && index + 1 == seeds.len() && token == "*" {
            token = KEYS[*seed as usize % KEYS.len()].to_owned();
        }
        out.push('/');
        out.push_str(&token);
    }
    out
}

fn conversion(seed: u32) -> (Conversion, Conversion) {
    match seed % 5 {
        0 => (Conversion::NumberToString, Conversion::StringToNumber),
        1 => (Conversion::Wrap, Conversion::Unwrap),
        2 => (Conversion::Nest { key: "v".into() }, Conversion::Unnest { key: "v".into() }),
        3 => (Conversion::BoolToNumber, Conversion::NumberToBool),
        _ => (Conversion::StringToNumber, Conversion::NumberToString),
    }
}

/// Rule `n` (tests/api_compat.rs `make_change` with every kind).
fn make_change(kind: u32, seeds: &[u32; 10], n: u32) -> Change {
    let depth = (seeds[0] % 3 + 1) as usize;
    let at = |last_not_wild| path(&seeds[1..=depth], n, last_not_wild);
    let fresh = format!("r{n}");
    match kind % 7 {
        0 => Change::Rename { path: at(true), to: fresh },
        1 => Change::Add {
            path: format!("{}/{fresh}", path(&seeds[1..depth], n, false)),
            default: json!(n),
        },
        2 => Change::RenameValue {
            path: at(false),
            from: json!(i64::from(seeds[5]) % 50 - 25),
            to: json!(format!("~v{n}")),
        },
        3 => {
            let (up, down) = conversion(seeds[6]);
            let (up, down) = if matches!(up, Conversion::Wrap | Conversion::Nest { .. }) {
                (up, down)
            } else {
                (Conversion::Wrap, Conversion::Unwrap)
            };
            Change::Retype { path: at(false), up, down }
        }
        4 => Change::Remove { path: at(true), default: json!(n) },
        5 => {
            let (up, down) = conversion(seeds[6]);
            Change::Retype { path: at(false), up, down }
        }
        _ => {
            let from = at(true);
            let mut tokens: Vec<&str> = from.split('/').skip(1).collect();
            tokens.truncate(seeds[7] as usize % tokens.len().max(1));
            let shared = tokens.len();
            let mut to: String = tokens.iter().map(|token| format!("/{token}")).collect();
            if seeds[8] % 2 == 1 {
                to.push_str(&format!("/{}", KEYS[seeds[9] as usize % KEYS.len()]));
            }
            to.push_str(&format!("/{fresh}"));
            // Wildcards must sit in the shared prefix; a rename otherwise.
            let from_tokens: Vec<&str> = from.split('/').skip(1).collect();
            let nested = to.starts_with(&format!("{from}/"));
            if nested || from_tokens.get(shared..).is_some_and(|rest| rest.contains(&"*")) {
                Change::Rename { path: from, to: fresh }
            } else {
                Change::Move { from, path: to }
            }
        }
    }
}

fn chain(rules: &[(u32, [u16; 10])]) -> Option<Compat> {
    let mut schema = Schema::new("fuzz", 0);
    for (n, (kind, seeds)) in rules.iter().take(MAX_RULES).enumerate() {
        let seeds = seeds.map(|seed| u32::from(seed) % 1000);
        let change = make_change(*kind, &seeds, u32::try_from(n).ok()?);
        schema.push("", vec![MessageChange { message: "m".into(), change }]);
    }
    Compat::new(schema).ok()
}

/// Spare bytes through `program`: never a panic; JSON in gives JSON out.
fn rewrite_spare(program: &cmux_api_compat::Program, spare: &[u8]) {
    let rewritten = program.rewrite(spare);
    if serde_json::from_slice::<Value>(spare).is_ok() {
        match rewritten {
            Ok(out) => assert!(
                serde_json::from_slice::<Value>(&out).is_ok(),
                "JSON rewrites to text that is not JSON: {}",
                String::from_utf8_lossy(&out)
            ),
            Err(error) => panic!("JSON input is refused: {error}"),
        }
    }
}

fn differential(rules: &[(u32, [u16; 10])], doc: &Doc, from: u8, to: u8, spare: &[u8]) {
    let Some(compat) = chain(rules) else { return };
    let top = compat.current();
    let (from, to) = (u32::from(from) % (top + 1), u32::from(to) % (top + 1));
    let program = compat.program("m", from, to).expect("a revision pair in range compiles");
    let doc = doc.value(0);
    let mut expected = doc.clone();
    program.apply_reference(&mut expected);
    for bytes in [serde_json::to_vec(&doc), serde_json::to_vec_pretty(&doc)] {
        let bytes = bytes.expect("a document encodes");
        let out = program.rewrite(&bytes).expect("the editor refuses a JSON document");
        let got: Value = serde_json::from_slice(&out).expect("the editor writes JSON");
        assert_eq!(got, expected, "stream and reference differ from {from} to {to}");
    }
    rewrite_spare(&program, spare);
}

fn schema_file(file: &[u8], spare: &[u8]) {
    let Ok(schema) = Schema::from_json(file) else { return };
    let kinds: Vec<String> = schema
        .revisions
        .iter()
        .flat_map(|revision| revision.changes.iter().map(|change| change.message.clone()))
        .collect();
    let Ok(compat) = Compat::new(schema) else { return };
    let (base, top) = (compat.schema().base, compat.current());
    for kind in kinds.iter().take(8) {
        for (from, to) in [(base, top), (top, base)] {
            if let Ok(program) = compat.program(kind, from, to) {
                let _ = program.rewrite(spare);
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    match data.split_first() {
        Some((0, rest)) => {
            let (file, message) =
                rest.split_at(rest.iter().position(|byte| *byte == 0).unwrap_or(rest.len()));
            schema_file(file, message.get(1..).unwrap_or_default());
        }
        _ => {
            if let Ok(chain) = Chain::arbitrary_take_rest(Unstructured::new(data)) {
                differential(&chain.rules, &chain.doc, chain.from, chain.to, &chain.spare);
            }
        }
    }
});
