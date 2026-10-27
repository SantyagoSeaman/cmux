//! Bulk replies (p05-ipc-149) on a writer that records what it sends: a
//! framed `payload: binary` request gets one REPLY frame with the binary
//! flag and its bytes raw; a line, or a REQUEST frame of a JSON method,
//! gets base64 in its JSON, counted in `ipc.base64_encodes` by transport.

use std::sync::Arc;

use cmux_threads::sync::Mutex;

use cmux_ipc::{CallAux, CallFlags, FrameHeader, MethodTables};
use serde_json::{Value, json};

use super::{BulkData, answer};
use crate::binary_body::BulkReply;
use crate::diagnostics::IpcStats;
use crate::server::frame_dispatch::FrameCall;
use crate::server::{BudgetedText, FramePayload, MessageSink, MessageWriter, OutboundStream};

/// What the writer sent: a line, or a frame and its payload bytes.
#[derive(Debug)]
pub(in crate::server) enum Sent {
    Line(Value),
    Frame(FrameHeader, Vec<u8>),
}

/// A sink that keeps every control message and frame it is handed.
#[derive(Default)]
pub(in crate::server) struct RecordingSink {
    pub(in crate::server) sent: Mutex<Vec<Sent>>,
}

impl RecordingSink {
    fn line(&self, text: &BudgetedText) -> std::io::Result<()> {
        let value = serde_json::from_str(text).map_err(std::io::Error::other)?;
        self.sent.lock().unwrap().push(Sent::Line(value));
        Ok(())
    }

    /// Everything sent so far.
    pub(in crate::server) fn take(&self) -> Vec<Sent> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }
}

impl MessageSink for Arc<RecordingSink> {
    fn send_initial(&self, text: Arc<BudgetedText>, _: &OutboundStream) -> std::io::Result<()> {
        self.line(&text)
    }
    fn send_stream(&self, text: Arc<BudgetedText>, _: &OutboundStream) -> std::io::Result<()> {
        self.line(&text)
    }
    fn send_control(&self, text: Arc<BudgetedText>) -> std::io::Result<()> {
        self.line(&text)
    }
    fn send_frame(&self, header: FrameHeader, payload: FramePayload) -> std::io::Result<()> {
        self.sent.lock().unwrap().push(Sent::Frame(header, payload.as_bytes().to_vec()));
        Ok(())
    }
    fn send_terminal(&self, text: Arc<BudgetedText>, _: &OutboundStream) -> std::io::Result<()> {
        self.line(&text)
    }
    fn send_ordered_terminal(
        &self,
        text: Arc<BudgetedText>,
        _: &OutboundStream,
    ) -> std::io::Result<()> {
        self.line(&text)
    }
    fn is_open(&self) -> bool {
        true
    }
    fn close(&self) {}
}

/// A writer answering a REQUEST frame of `name` (id 7, stream 0): a binary
/// one carrying `raw` after its meta when `raw` is set.
pub(in crate::server) fn framed_writer(
    name: &str,
    raw: Option<&[u8]>,
) -> (MessageWriter, Arc<RecordingSink>) {
    let tables = MethodTables::schema();
    let method = tables.v12_id(name).and_then(|id| tables.method(id)).expect("a v12 method");
    let flags = if raw.is_some() { CallFlags::BINARY } else { CallFlags::empty() };
    let call =
        CallAux { request_id: 7, method_id: method.id.raw(), priority: method.priority, flags };
    let call = FrameCall::new(0, call);
    let call = match raw {
        Some(raw) => call.binary(bytes::Bytes::copy_from_slice(raw)),
        None => call,
    };
    let sink = Arc::new(RecordingSink::default());
    let writer = MessageWriter::new(sink.clone()).replying_to(Arc::new(call));
    (writer, sink)
}

/// A reply of two byte fields, for the transport rules alone.
struct TwoParts(Vec<u8>, Vec<u8>);

impl BulkReply for TwoParts {
    fn into_json(self) -> Value {
        use base64::Engine as _;
        let engine = &base64::engine::general_purpose::STANDARD;
        json!({"a": engine.encode(self.0), "b": engine.encode(self.1)})
    }

    fn body_parts(&self) -> (Value, Vec<&[u8]>) {
        (json!({"a_len": self.0.len(), "b_len": self.1.len()}), vec![&self.0, &self.1])
    }
}

fn envelope(data: Value) -> Value {
    super::ok_envelope(Some(json!(7)), data)
}

#[test]
fn bulk_reply_a_framed_binary_request_gets_its_bytes_raw_and_counts_no_base64() {
    let ipc = IpcStats::default();
    let (writer, sink) = framed_writer("fs.read", Some(b""));
    let reply = TwoParts(vec![0, 1, 2, 0xFF], b"tail".to_vec());
    assert!(matches!(answer(&ipc, &writer, reply, envelope), BulkData::Sent(true)));
    let sent = sink.take();
    let [Sent::Frame(header, body)] = sent.as_slice() else { panic!("{sent:?}") };
    let call = header.call().unwrap();
    assert_eq!((call.request_id, call.flags), (7, CallFlags::BINARY));
    let mut read = crate::binary_body::BinaryBody::decode(body).unwrap();
    let mut data = read.meta.remove("data").unwrap();
    let data = data.as_object_mut().unwrap();
    let a_len = crate::binary_body::take_len(data, "a_len").unwrap();
    assert_eq!(read.take_bytes(a_len).unwrap(), [0, 1, 2, 0xFF]);
    let b_len = crate::binary_body::take_len(data, "b_len").unwrap();
    assert_eq!(read.take_bytes(b_len).unwrap(), b"tail");
    assert_eq!(Value::Object(read.finish().unwrap()), json!({"id": 7, "ok": true}));
    let counted = ipc.snapshot().base64_encodes;
    assert_eq!((counted.lines, counted.frames), (0, 0));
    // Its one reply is out: nothing more is binary.
    assert!(!writer.answers_binary());
}

#[test]
fn bulk_reply_a_line_or_a_json_frame_gets_base64_counted_by_its_transport() {
    let ipc = IpcStats::default();
    let reply = || TwoParts(vec![1, 2, 3], vec![4]);

    let (line, sink) = framed_writer("ping", None);
    let line = line.connection_writer();
    let BulkData::Json(data) = answer(&ipc, &line, reply(), envelope) else { panic!("binary") };
    assert_eq!(data, reply().into_json());
    assert!(sink.take().is_empty(), "the caller sends the line");

    // A REQUEST frame of a JSON method still takes the JSON path.
    let (framed, _) = framed_writer("ping", None);
    assert!(matches!(answer(&ipc, &framed, reply(), envelope), BulkData::Json(_)));
    let counted = ipc.snapshot().base64_encodes;
    assert_eq!((counted.lines, counted.frames), (1, 1));
}

#[test]
fn bulk_reply_a_binary_reply_refuses_a_writer_with_no_binary_request() {
    let (writer, sink) = framed_writer("ping", None);
    let error = writer.send_binary_reply(&json!({"ok": true}), &[b"x"]).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(sink.take().is_empty());
}
