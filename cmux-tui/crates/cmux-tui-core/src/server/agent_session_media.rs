//! `agent-session-media`: one image of the attached chat, read from its host
//! acpmux in ranges, answered as its JSON line (`{mimeType, data}`, the
//! bytes in base64) or as a binary body (crate::binary_body, p05-ipc-149)
//! whose raw bytes are the image itself.

use base64::Engine as _;
use serde_json::{Value, json};

use super::{Attachment, CALL_TIMEOUT, Refusal};
use crate::binary_body::BulkReply;

/// The largest image, and the largest range one acpmux call returns.
const MAX_BYTES: u64 = 10 << 20;
const MAX_RANGE: u64 = 6 << 20;

/// An `agent-session-media` answer: the image's MIME type and its bytes.
pub(super) struct MediaReply {
    mime_type: Value,
    bytes: Vec<u8>,
}

impl BulkReply for MediaReply {
    /// `{mimeType, data}` with `data` in base64.
    fn into_json(self) -> Value {
        json!({
            "mimeType": self.mime_type,
            "data": base64::engine::general_purpose::STANDARD.encode(self.bytes),
        })
    }

    /// `{mimeType, data_len}`, then the image raw.
    fn body_parts(&self) -> (Value, Vec<&[u8]>) {
        let meta = json!({"mimeType": self.mime_type, "data_len": self.bytes.len()});
        (meta, vec![self.bytes.as_slice()])
    }
}

/// Reads one image from the attached chat's host acpmux. The attachment
/// daemon keeps the path scoped to the bound session; this command only
/// relays the already checked media reader bytes.
pub(super) fn media(attachment: &Attachment, path: String) -> Result<MediaReply, Refusal> {
    let image_extension = std::path::Path::new(&path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        });
    if !image_extension {
        return Err(Refusal::BadRequest);
    }
    if path.is_empty() || path.len() > 4096 {
        return Err(Refusal::BadRequest);
    }
    let opened = attachment
        .link
        .call(
            "_acpmux/media_open",
            json!({"sessionId": attachment.session, "path": path}),
            CALL_TIMEOUT,
        )
        .map_err(Refusal::of)?;
    let handle = opened.get("handle").and_then(Value::as_str).ok_or(Refusal::BadRequest)?;
    let size = opened.get("size").and_then(Value::as_u64).ok_or(Refusal::BadRequest)?;
    if size == 0 || size > MAX_BYTES {
        return Err(Refusal::BadRequest);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).map_err(|_| Refusal::BadRequest)?);
    let mut offset = 0u64;
    while offset < size {
        let length = (size - offset).min(MAX_RANGE);
        let range = attachment
            .link
            .call(
                "_acpmux/media_range",
                json!({"sessionId": attachment.session, "handle": handle, "offset": offset, "length": length}),
                CALL_TIMEOUT,
            )
            .map_err(Refusal::of)?;
        let actual = range.get("offset").and_then(Value::as_u64).ok_or(Refusal::BadRequest)?;
        if actual != offset {
            return Err(Refusal::BadRequest);
        }
        // acpmux's own JSON-RPC carries a range as base64; it is decoded
        // once here and never encoded again on a frame.
        let encoded = range.get("data").and_then(Value::as_str).ok_or(Refusal::BadRequest)?;
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| Refusal::BadRequest)?;
        if chunk.is_empty() || chunk.len() as u64 > length {
            return Err(Refusal::BadRequest);
        }
        offset = offset.saturating_add(chunk.len() as u64);
        bytes.extend_from_slice(&chunk);
    }
    Ok(MediaReply { mime_type: opened.get("mimeType").cloned().unwrap_or(Value::Null), bytes })
}

#[cfg(test)]
mod tests {
    use super::MediaReply;
    use crate::binary_body::BulkReply as _;
    use base64::Engine as _;
    use serde_json::{Value, json};

    /// p05-ipc-149: the binary body holds the image raw (no base64) and
    /// stands for the line byte for byte.
    #[test]
    fn agent_session_media_binary_body_equals_the_line() {
        let png: Vec<u8> =
            b"\x89PNG\r\n\x1a\n".iter().copied().chain((0..=255u8).cycle().take(9000)).collect();
        for (mime_type, bytes) in [(json!("image/png"), png), (Value::Null, vec![0u8])] {
            let reply = || MediaReply { mime_type: mime_type.clone(), bytes: bytes.clone() };
            let line = reply().into_json();
            let body = crate::binary_body::body_of(&reply());
            let mut read = crate::binary_body::BinaryBody::decode(&body).unwrap();
            let raw = read.take("data_len").unwrap();
            assert_eq!(raw, bytes.as_slice());
            let data = base64::engine::general_purpose::STANDARD.encode(raw);
            let mut rebuilt = read.finish().unwrap();
            rebuilt.insert("data".into(), json!(data));
            let rebuilt = Value::Object(rebuilt);
            assert_eq!(rebuilt, line);
            assert_eq!(serde_json::to_vec(&rebuilt).unwrap(), serde_json::to_vec(&line).unwrap());
            assert!(!String::from_utf8_lossy(&body).contains("\"data\""));
        }
    }
}
