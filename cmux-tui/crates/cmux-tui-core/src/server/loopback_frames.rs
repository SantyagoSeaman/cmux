//! `loopback-data` as JSON lines or as binary stream frames (p05-ipc-149;
//! plans/cmux-next/chrome-parity/p05-ipc.md 3.2).
//!
//! On a line the bytes are base64 text. On a stream granted frames the
//! target's bytes go as one OUTPUT frame on the forward's frame stream whose
//! payload is the bytes themselves and whose aux is the absolute offset of
//! its first byte in what the target sent, as attach OUTPUT carries a PTY
//! offset (server/attach_frames.rs): offsets of consecutive frames are
//! contiguous, so a reader that sees a gap knows bytes were lost. The
//! client's bytes come back as INPUT frames, read here without a text
//! decode. Chrome moves a socket's bytes through a Mojo data pipe, never as
//! text (chromium services/network/tcp_connected_socket.cc).

use std::fmt;

use base64::Engine as _;
use cmux_ipc::{Aux, BadMessage, Frame, FrameKind};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use serde_json::{Value, json};

use super::{ForwardStream, LoopbackForwarder, MAX_FRAME_BYTES};

/// Bytes the target sent on one forward: the `loopback-data` event, as a
/// line or as an OUTPUT frame.
pub(super) struct LoopbackData<'a> {
    /// The forward's `stream` id.
    pub(super) stream: u64,
    /// Bytes the target sent on this forward before these.
    pub(super) offset: u64,
    pub(super) bytes: &'a [u8],
}

impl LoopbackData<'_> {
    /// The event line: `{event, stream, data}` with `data` in base64.
    pub(super) fn json(&self) -> Value {
        let data = base64::engine::general_purpose::STANDARD.encode(self.bytes);
        json!({"event": "loopback-data", "stream": self.stream, "data": data})
    }

    /// One OUTPUT frame on `frame_stream` (never stream 0): the bytes raw,
    /// aux the offset of the first.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "loopback forwards ride client-minted frame streams once their opener is framed"
        )
    )]
    pub(super) fn frame(&self, frame_stream: u16) -> Result<Frame, BadMessage> {
        Frame::new(
            FrameKind::Output,
            frame_stream,
            Aux::Offset(self.offset),
            bytes::Bytes::copy_from_slice(self.bytes),
        )
    }
}

/// A client's `loopback-data` payload. Text that is not base64, or bytes
/// past [`MAX_FRAME_BYTES`], are `Bad`, so the data still reaches its
/// forward and the forward ends with `bad-frame`.
pub(in crate::server) enum DataPayload {
    Bytes(Vec<u8>),
    Bad,
}

impl DataPayload {
    /// The longest padded base64 text of at most [`MAX_FRAME_BYTES`].
    const MAX_TEXT_BYTES: usize = MAX_FRAME_BYTES.div_ceil(3) * 4;

    /// A line's `data`, decoded straight into the bytes the forward queues
    /// for its target, never through a `String` copy first.
    fn from_base64(text: &str) -> Self {
        if text.len() > Self::MAX_TEXT_BYTES {
            return Self::Bad;
        }
        match base64::engine::general_purpose::STANDARD.decode(text.as_bytes()) {
            Ok(bytes) if bytes.len() <= MAX_FRAME_BYTES => Self::Bytes(bytes),
            _ => Self::Bad,
        }
    }

    /// An INPUT frame's payload: the bytes as they came.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "loopback forwards ride client-minted frame streams once their opener is framed"
        )
    )]
    pub(super) fn from_frame(payload: &[u8]) -> Self {
        if payload.len() > MAX_FRAME_BYTES {
            return Self::Bad;
        }
        Self::Bytes(payload.to_vec())
    }

    /// Queues the bytes for `target`'s socket, from a line or a frame alike;
    /// a bad payload ends the forward with `bad-frame`.
    pub(super) fn deliver(self, target: &ForwardStream, forwarder: &LoopbackForwarder) {
        match self {
            Self::Bytes(bytes) if bytes.is_empty() => {}
            Self::Bytes(bytes) => target.accept_data(forwarder, bytes),
            Self::Bad => target.fail(forwarder, "bad-frame", true),
        }
    }
}

impl<'de> Deserialize<'de> for DataPayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PayloadVisitor;
        impl Visitor<'_> for PayloadVisitor {
            type Value = DataPayload;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a base64 string")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                Ok(DataPayload::from_base64(text))
            }
        }
        deserializer.deserialize_str(PayloadVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{DataPayload, LoopbackData, MAX_FRAME_BYTES};
    use base64::Engine as _;
    use cmux_ipc::{Aux, FrameKind};
    use serde_json::json;

    /// p05-ipc-149: the target's bytes cut into OUTPUT frames come back
    /// byte for byte, with contiguous offsets, and equal the bytes of the
    /// `loopback-data` lines; no frame holds base64.
    #[test]
    fn loopback_forward_output_frames_equal_the_lines_with_contiguous_offsets() {
        let sent: Vec<u8> = (0..=255u8).cycle().take(3 * MAX_FRAME_BYTES + 17).collect();
        let (mut offset, mut from_frames, mut from_lines) = (0u64, Vec::new(), Vec::new());
        for chunk in sent.chunks(MAX_FRAME_BYTES) {
            let data = LoopbackData { stream: 9, offset, bytes: chunk };
            let frame = data.frame(4).unwrap();
            assert_eq!(frame.header().kind(), FrameKind::Output);
            assert_eq!(frame.header().stream(), 4);
            assert_eq!(frame.header().aux(), Aux::Offset(from_frames.len() as u64));
            assert_eq!(frame.payload().as_ref(), chunk, "the payload is the bytes, not base64");
            from_frames.extend_from_slice(frame.payload());

            let line = data.json();
            assert_eq!(line["event"], "loopback-data");
            assert_eq!(line["stream"], 9);
            let text = line["data"].as_str().unwrap();
            from_lines.extend(base64::engine::general_purpose::STANDARD.decode(text).unwrap());
            offset += chunk.len() as u64;
        }
        assert_eq!(from_frames, sent);
        assert_eq!(from_lines, sent);
        assert!(LoopbackData { stream: 1, offset: 0, bytes: b"x" }.frame(0).is_err());
    }

    /// p05-ipc-149: an INPUT frame's payload is queued as it came; one past
    /// the frame bound is `Bad`, as an oversized base64 line is.
    #[test]
    fn loopback_forward_input_frames_carry_the_bytes_of_the_line() {
        let bytes: Vec<u8> = (0..=255u8).rev().cycle().take(MAX_FRAME_BYTES).collect();
        let text = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let line: DataPayload = serde_json::from_value(json!(text)).unwrap();
        let (DataPayload::Bytes(from_line), DataPayload::Bytes(from_frame)) =
            (line, DataPayload::from_frame(&bytes))
        else {
            panic!("a full frame of bytes is good on both paths");
        };
        assert_eq!(from_line, bytes);
        assert_eq!(from_frame, bytes);
        let over = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(DataPayload::from_frame(&over), DataPayload::Bad));
        let over = base64::engine::general_purpose::STANDARD.encode(&over);
        assert!(matches!(serde_json::from_value(json!(over)).unwrap(), DataPayload::Bad));
        assert!(matches!(serde_json::from_value(json!("not base64!")).unwrap(), DataPayload::Bad));
    }
}
