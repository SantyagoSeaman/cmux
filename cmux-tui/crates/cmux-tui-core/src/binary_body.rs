//! Binary request and reply bodies for bulk bytes on frames (p05-ipc-149;
//! plans/cmux-next/chrome-parity/p05-ipc.md 3.3, `payload: binary`).
//!
//! ```text
//! layout u8 | meta_len u32 LE | meta: one JSON object, meta_len bytes | raw
//! ```
//!
//! The meta is the op's JSON line without its base64 fields, plus each
//! field's decoded length (`data_len`); raw holds those bytes in the order
//! the meta names them. It is the SNAPSHOT/REPLAY payload
//! (server/snapshot_frames.rs) behind the schema's layout version byte, so a
//! reader turns it back into the line byte for byte, and the bytes are never
//! base64-encoded: Chrome moves large bytes as a `mojo_base::BigBuffer`, never
//! as text (chromium mojo/public/cpp/base/big_buffer.h).

use std::fmt;

use cmux_ipc::MAX_FRAME_PAYLOAD;
use serde_json::{Map, Value};

/// The layout version byte this daemon writes and reads.
pub(crate) const LAYOUT_VERSION: u8 = 1;
/// Bytes before the meta: the layout byte and `meta_len`.
pub(crate) const HEADER_BYTES: usize = 1 + 4;

/// Why a binary body could not be written or read.
#[derive(Debug)]
pub(crate) enum BinaryBodyError {
    /// The meta did not serialize, or is not one JSON object.
    Meta(String),
    /// The body would pass `MAX_FRAME_PAYLOAD`.
    Oversize { len: usize },
    /// A layout this daemon does not read.
    Layout { version: u8 },
    /// The body ends before the length its header or meta names.
    Truncated { have: usize, need: usize },
    /// A `<field>_len` that is missing or not a byte count.
    Length { field: &'static str },
    /// Raw bytes the meta does not name.
    Trailing { bytes: usize },
}

impl fmt::Display for BinaryBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Meta(error) => write!(f, "binary body meta: {error}"),
            Self::Oversize { len } => {
                write!(f, "binary body of {len} bytes is over {MAX_FRAME_PAYLOAD}")
            }
            Self::Layout { version } => {
                write!(f, "binary body layout {version} is not {LAYOUT_VERSION}")
            }
            Self::Truncated { have, need } => {
                write!(f, "binary body has {have} bytes where {need} are named")
            }
            Self::Length { field } => write!(f, "binary body meta has no byte count {field}"),
            Self::Trailing { bytes } => {
                write!(f, "binary body has {bytes} bytes its meta does not name")
            }
        }
    }
}

impl std::error::Error for BinaryBodyError {}

/// `meta` and `raw` as one body. `meta` must be a JSON object.
pub(crate) fn encode(meta: &Value, raw: &[&[u8]]) -> Result<Vec<u8>, BinaryBodyError> {
    if !meta.is_object() {
        return Err(BinaryBodyError::Meta("not an object".into()));
    }
    let meta =
        serde_json::to_vec(meta).map_err(|error| BinaryBodyError::Meta(error.to_string()))?;
    let len = raw
        .iter()
        .try_fold(HEADER_BYTES.saturating_add(meta.len()), |len, part| len.checked_add(part.len()))
        .unwrap_or(usize::MAX);
    if len > MAX_FRAME_PAYLOAD as usize {
        return Err(BinaryBodyError::Oversize { len });
    }
    let meta_len = u32::try_from(meta.len()).map_err(|_| BinaryBodyError::Oversize { len })?;
    let mut body = Vec::with_capacity(len);
    body.push(LAYOUT_VERSION);
    body.extend_from_slice(&meta_len.to_le_bytes());
    body.extend_from_slice(&meta);
    for part in raw {
        body.extend_from_slice(part);
    }
    Ok(body)
}

/// A body read back: its meta object and the raw bytes still unread.
#[derive(Debug)]
pub(crate) struct BinaryBody<'a> {
    pub(crate) meta: Map<String, Value>,
    raw: &'a [u8],
}

impl<'a> BinaryBody<'a> {
    /// Splits `body` into its meta and raw bytes; nothing is copied but the
    /// meta's parse.
    pub(crate) fn decode(body: &'a [u8]) -> Result<Self, BinaryBodyError> {
        let (&version, rest) =
            body.split_first().ok_or(BinaryBodyError::Truncated { have: 0, need: HEADER_BYTES })?;
        if version != LAYOUT_VERSION {
            return Err(BinaryBodyError::Layout { version });
        }
        let (meta_len, rest) = rest
            .split_first_chunk::<4>()
            .ok_or(BinaryBodyError::Truncated { have: body.len(), need: HEADER_BYTES })?;
        let meta_len = usize::try_from(u32::from_le_bytes(*meta_len)).unwrap_or(usize::MAX);
        let (meta, raw) = rest.split_at_checked(meta_len).ok_or(BinaryBodyError::Truncated {
            have: body.len(),
            need: HEADER_BYTES.saturating_add(meta_len),
        })?;
        let meta = match serde_json::from_slice(meta) {
            Ok(Value::Object(meta)) => meta,
            Ok(_) => return Err(BinaryBodyError::Meta("not an object".into())),
            Err(error) => return Err(BinaryBodyError::Meta(error.to_string())),
        };
        Ok(Self { meta, raw })
    }

    /// Removes `field` (a byte count) from the meta and takes that many raw
    /// bytes, in the order the fields were written.
    pub(crate) fn take(&mut self, field: &'static str) -> Result<&'a [u8], BinaryBodyError> {
        let len = take_len(&mut self.meta, field)?;
        self.take_bytes(len)
    }

    /// The next `len` raw bytes, for a byte count the caller read from the
    /// meta itself (one inside a nested object).
    pub(crate) fn take_bytes(&mut self, len: usize) -> Result<&'a [u8], BinaryBodyError> {
        let (taken, rest) = self
            .raw
            .split_at_checked(len)
            .ok_or(BinaryBodyError::Truncated { have: self.raw.len(), need: len })?;
        self.raw = rest;
        Ok(taken)
    }

    /// The meta, once every raw byte was taken.
    pub(crate) fn finish(self) -> Result<Map<String, Value>, BinaryBodyError> {
        if !self.raw.is_empty() {
            return Err(BinaryBodyError::Trailing { bytes: self.raw.len() });
        }
        Ok(self.meta)
    }
}

/// Removes the byte count `field` from `meta`.
pub(crate) fn take_len(
    meta: &mut Map<String, Value>,
    field: &'static str,
) -> Result<usize, BinaryBodyError> {
    meta.remove(field)
        .and_then(|len| len.as_u64())
        .and_then(|len| usize::try_from(len).ok())
        .ok_or(BinaryBodyError::Length { field })
}

#[cfg(test)]
mod tests {
    use super::{BinaryBody, BinaryBodyError, HEADER_BYTES, LAYOUT_VERSION, encode};
    use serde_json::json;

    #[test]
    fn binary_body_round_trips_meta_and_raw_in_order() {
        let body = encode(&json!({"a_len": 3, "b_len": 2, "x": "y"}), &[b"abc", b"de"]).unwrap();
        assert_eq!(body[0], LAYOUT_VERSION);
        let mut read = BinaryBody::decode(&body).unwrap();
        assert_eq!(read.take("a_len").unwrap(), b"abc");
        assert_eq!(read.take("b_len").unwrap(), b"de");
        assert_eq!(serde_json::Value::Object(read.finish().unwrap()), json!({"x": "y"}));
    }

    #[test]
    fn binary_body_refuses_bad_layouts_lengths_and_trailing_bytes() {
        let body = encode(&json!({"a_len": 1}), &[b"ab"]).unwrap();
        let mut read = BinaryBody::decode(&body).unwrap();
        assert_eq!(read.take("a_len").unwrap(), b"a");
        assert!(matches!(read.finish(), Err(BinaryBodyError::Trailing { bytes: 1 })));

        let mut read = BinaryBody::decode(&body).unwrap();
        assert!(matches!(read.take("b_len"), Err(BinaryBodyError::Length { field: "b_len" })));

        let short = encode(&json!({"a_len": 9}), &[b"ab"]).unwrap();
        assert!(matches!(
            BinaryBody::decode(&short).unwrap().take("a_len"),
            Err(BinaryBodyError::Truncated { have: 2, need: 9 })
        ));

        let mut other = body.clone();
        other[0] = LAYOUT_VERSION + 1;
        assert!(matches!(BinaryBody::decode(&other), Err(BinaryBodyError::Layout { .. })));
        assert!(matches!(
            BinaryBody::decode(&body[..HEADER_BYTES + 2]),
            Err(BinaryBodyError::Truncated { .. })
        ));
        assert!(matches!(encode(&json!([1]), &[]), Err(BinaryBodyError::Meta(_))));
    }
}
