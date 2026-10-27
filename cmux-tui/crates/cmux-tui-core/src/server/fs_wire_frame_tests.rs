//! `fs.read` and `fs.write` answering REQUEST frames (p05-ipc-149): a
//! framed `fs.read` gets the file's bytes raw in a binary REPLY and stands
//! for its line byte for byte; a framed `fs.write` writes the raw bytes after
//! its meta; neither base64-encodes (`ipc.base64_encodes.frames` stays 0).

use cmux_ipc::CallFlags;
use serde_json::{Map, Value, json};

use super::answer_route;
use super::tests::Home;
use crate::diagnostics::IpcStats;
use crate::server::bulk_reply::tests::{Sent, framed_writer};

fn request(value: Value) -> Map<String, Value> {
    let Value::Object(request) = value else { panic!("{value}") };
    request
}

/// The line a binary `fs.read` reply stands for: its `data_len` bytes back
/// as `text` or `bytes_base64`, as `encoding` says.
fn line_of(body: &[u8]) -> Value {
    use base64::Engine as _;
    let mut read = crate::binary_body::BinaryBody::decode(body).unwrap();
    let mut data = read.meta.remove("data").unwrap();
    let data_object = data.as_object_mut().unwrap();
    let len = crate::binary_body::take_len(data_object, "data_len").unwrap();
    let bytes = read.take_bytes(len).unwrap().to_vec();
    let (field, value) = match data_object["encoding"].as_str() {
        Some("utf-8") => ("text", json!(String::from_utf8(bytes).unwrap())),
        _ => ("bytes_base64", json!(base64::engine::general_purpose::STANDARD.encode(bytes))),
    };
    data_object.insert(field.into(), value);
    let mut line = read.finish().unwrap();
    line.insert("data".into(), data);
    Value::Object(line)
}

#[test]
fn fs_wire_a_framed_read_answers_raw_bytes_equal_to_its_line() {
    let home = Home::new("frame-read");
    let service = home.service();
    let ipc = IpcStats::default();
    let binary: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    for (name, bytes) in [("text", b"hello\nworld".to_vec()), ("binary", binary)] {
        std::fs::write(home.path.join(name), &bytes).unwrap();
        let path = home.path.join(name).display().to_string();
        let body = json!({"id": 3, "cmd": "fs.read", "path": path, "max_bytes": 1 << 20});

        let (framed, sink) = framed_writer("fs.read", Some(b""));
        assert!(answer_route(&ipc, &framed, Some(&service), Some("owner"), request(body.clone())));
        let sent = sink.take();
        let [Sent::Frame(header, reply)] = sent.as_slice() else { panic!("{sent:?}") };
        assert_eq!(header.call().unwrap().flags, CallFlags::BINARY, "{name}");
        assert!(reply.ends_with(&bytes), "{name}: the bytes are raw");

        let (line, sink) = framed_writer("fs.read", None);
        let line = line.connection_writer();
        assert!(answer_route(&ipc, &line, Some(&service), Some("owner"), request(body)));
        let sent = sink.take();
        let [Sent::Line(line)] = sent.as_slice() else { panic!("{sent:?}") };
        let rebuilt = line_of(reply);
        assert_eq!(&rebuilt, line, "{name}");
        assert_eq!(serde_json::to_vec(&rebuilt).unwrap(), serde_json::to_vec(line).unwrap());
    }
    let counted = ipc.snapshot().base64_encodes;
    assert_eq!((counted.lines, counted.frames), (2, 0));
}

#[test]
fn fs_wire_a_framed_write_writes_the_raw_bytes_after_its_meta() {
    let home = Home::new("frame-write");
    let service = home.service();
    let ipc = IpcStats::default();
    let bytes: Vec<u8> = (0..=255u8).rev().cycle().take(7000).collect();
    let path = home.path.join("framed").display().to_string();
    let body = json!({"id": 4, "cmd": "fs.write", "path": path, "mode": "create"});
    let (framed, sink) = framed_writer("fs.write", Some(&bytes));
    assert!(answer_route(&ipc, &framed, Some(&service), Some("owner"), request(body)));
    assert_eq!(std::fs::read(home.path.join("framed")).unwrap(), bytes);
    let sent = sink.take();
    let [Sent::Frame(header, reply)] = sent.as_slice() else { panic!("{sent:?}") };
    // The answer has no bulk bytes: a JSON REPLY.
    assert_eq!(header.call().unwrap().flags, CallFlags::empty());
    let reply: Value = serde_json::from_slice(reply).unwrap();
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert_eq!(reply["data"]["entry"]["size"], json!(bytes.len()), "{reply}");

    // A meta that also carries the bytes as text is refused, as an ERROR.
    let both = json!({"id": 5, "cmd": "fs.write", "path": path, "mode": "overwrite", "text": "x"});
    let (framed, sink) = framed_writer("fs.write", Some(b"y"));
    assert!(answer_route(&ipc, &framed, Some(&service), Some("owner"), request(both)));
    let sent = sink.take();
    let [Sent::Frame(header, reply)] = sent.as_slice() else { panic!("{sent:?}") };
    assert_eq!(header.kind(), cmux_ipc::FrameKind::Error);
    let reply: Value = serde_json::from_slice(reply).unwrap();
    assert_eq!(reply["error_code"], json!("params.invalid"), "{reply}");
    assert_eq!(std::fs::read(home.path.join("framed")).unwrap(), bytes);
    let counted = ipc.snapshot().base64_encodes;
    assert_eq!((counted.lines, counted.frames), (0, 0));
}
