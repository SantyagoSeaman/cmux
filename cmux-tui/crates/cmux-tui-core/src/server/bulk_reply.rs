//! Replies with bulk bytes (terminal-history, fs.read, agent-session-media;
//! p05-ipc-149): a framed request of a `payload: binary` method gets them
//! raw in a binary body, any other request as base64 in its JSON, counted
//! in `server-stats` `ipc` `base64_encodes` by the request's transport.

use serde_json::Value;

use super::{FramePayload, MessageWriter};
use crate::binary_body::BulkReply;
use crate::diagnostics::{IpcStats, IpcTransport};

impl MessageWriter {
    /// Whether this writer answers a framed `payload: binary` request that
    /// has no reply yet: its successful reply is a binary body
    /// ([`Self::send_binary_reply`]), never base64 in JSON.
    pub(super) fn answers_binary(&self) -> bool {
        self.reply.as_ref().is_some_and(|call| call.is_binary() && !call.is_answered())
    }

    /// How the request this writer answers arrived: a REQUEST frame or a
    /// line (`server-stats` `ipc` counts by it).
    pub(super) fn request_transport(&self) -> IpcTransport {
        if self.reply.is_some() { IpcTransport::Frames } else { IpcTransport::Lines }
    }

    /// The successful reply to the framed `payload: binary` request this
    /// writer answers: one REPLY frame with the binary flag whose payload is
    /// `meta` (the reply's JSON without its bulk bytes, with their `_len`s)
    /// and then `raw` (crate::binary_body). The bytes are copied once and
    /// never base64-encoded, as Chrome moves large bytes as a
    /// `mojo_base::BigBuffer` (mojo/public/cpp/base/big_buffer.h).
    pub(super) fn send_binary_reply(&self, meta: &Value, raw: &[&[u8]]) -> std::io::Result<()> {
        if !self.answers_binary() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a binary reply answers only a framed payload: binary request",
            ));
        }
        if !self.is_open() {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "connection closed"));
        }
        let result = crate::binary_body::encode(meta, raw)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            .and_then(|body| self.render_service.control_bytes(body))
            .and_then(|bytes| self.send_framed_reply(FramePayload::Bytes(bytes), true));
        if result.is_err() {
            self.close();
        }
        result
    }
}

/// How a bulk reply's `data` goes out.
pub(super) enum BulkData {
    /// Sent as a binary body; whether it was queued.
    Sent(bool),
    /// The line's `data`, for the caller to send as it sends every reply.
    Json(Value),
}

/// Answers the request `writer` answers with `reply`: the REPLY frame of a
/// framed `payload: binary` request, whose meta is `envelope` (the reply
/// line's object) around the body's `data`; for any other request the
/// line's `data`.
pub(super) fn answer<R: BulkReply>(
    ipc: &IpcStats,
    writer: &MessageWriter,
    reply: R,
    envelope: impl FnOnce(Value) -> Value,
) -> BulkData {
    if writer.answers_binary() {
        let (meta, raw) = reply.body_parts();
        return BulkData::Sent(writer.send_binary_reply(&envelope(meta), &raw).is_ok());
    }
    BulkData::Json(json_data(ipc, writer, reply))
}

/// The reply object of a request `id` that succeeded with `data`, as
/// `Response` writes it (`id` only when the request had one).
pub(super) fn ok_envelope(id: Option<Value>, data: Value) -> Value {
    let mut response = serde_json::Map::new();
    if let Some(id) = id {
        response.insert("id".into(), id);
    }
    response.insert("ok".into(), Value::Bool(true));
    response.insert("data".into(), data);
    Value::Object(response)
}

/// The line's `data` of `reply`, its bytes base64-encoded: counted under the
/// transport of the request `writer` answers.
pub(super) fn json_data<R: BulkReply>(ipc: &IpcStats, writer: &MessageWriter, reply: R) -> Value {
    ipc.base64_encoded(writer.request_transport());
    reply.into_json()
}

#[cfg(test)]
#[path = "bulk_reply_tests.rs"]
pub(super) mod tests;
