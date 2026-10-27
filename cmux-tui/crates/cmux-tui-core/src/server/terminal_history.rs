//! `terminal-history` and `terminal-read-range` (raw v12 forms of the
//! `terminal.history` and `terminal.read_range` ops in
//! `.cmux-scratch/nx-worker/cli-requests/terminal-snapshot-history.md`).
//!
//! Rows are addressed by host row markers (see ghostty-vt
//! `terminal_history.rs`): a marker stays on its row while output scrolls,
//! and reads of rows that left scrollback answer `range_evicted`. Markers
//! belong to a `marker_epoch`; a reflow starts a new epoch.

use std::sync::Arc;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use super::terminal_io_wire::{TerminalHistoryParams, TerminalReadRangeParams, wire_usize};
use super::{get_surface, require_pty};
use crate::Mux;
use crate::panic_free::JsonFields as _;

/// Default and maximum page bytes of one `terminal-history` reply.
pub const TERMINAL_HISTORY_DEFAULT_BYTES: usize = 1 << 20;
pub const TERMINAL_HISTORY_MAX_BYTES: usize = 8 << 20;

/// One end of a `terminal-read-range` range (schema `RowMarkerPoint`).
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct RowMarkerPoint {
    row_marker: u64,
    col: u16,
}

/// Cut `text` to at most `max_bytes` at a character boundary.
fn truncate_text(mut text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

fn marker_error(error: ghostty_vt::MarkerError) -> anyhow::Error {
    match error {
        ghostty_vt::MarkerError::RangeEvicted => anyhow::anyhow!("range_evicted"),
        ghostty_vt::MarkerError::Invalid => anyhow::anyhow!("invalid: range is outside the screen"),
        ghostty_vt::MarkerError::Vt(error) => anyhow::anyhow!("terminal snapshot failed: {error}"),
    }
}

pub(crate) fn history(mux: &Arc<Mux>, params: TerminalHistoryParams) -> anyhow::Result<Value> {
    let surface = get_surface(mux, params.surface)?;
    require_pty(&surface)?;
    let max_bytes = params.max_bytes.map_or(TERMINAL_HISTORY_DEFAULT_BYTES, wire_usize);
    anyhow::ensure!(
        (1..=TERMINAL_HISTORY_MAX_BYTES).contains(&max_bytes),
        "invalid: max_bytes must be 1..={TERMINAL_HISTORY_MAX_BYTES}"
    );
    let pages = surface
        .history_pages(Some(params.marker_epoch), params.before, max_bytes)
        .map_err(marker_error)?;
    Ok(HistoryReply::new(json!(params.surface), pages).into_json())
}

/// A `terminal-history` answer, as its JSON line or as its binary body
/// (crate::binary_body, p05-ipc-149).
pub(crate) struct HistoryReply {
    surface: Value,
    pages: ghostty_vt::HistoryPages,
}

impl HistoryReply {
    pub(crate) fn new(surface: Value, pages: ghostty_vt::HistoryPages) -> Self {
        Self { surface, pages }
    }

    /// The answer's fields but the page records; each page has `data_len`
    /// in place of `data` when `lengths` is set.
    fn meta(&self, lengths: bool) -> Value {
        let pages = &self.pages;
        json!({
            "surface": self.surface,
            "marker_epoch": pages.marker_epoch,
            "snapshot_version": ghostty_vt::snapshot_version(),
            "pages": pages.pages.iter().map(|page| {
                let mut entry = json!({"marker": page.marker, "rows": page.rows});
                if lengths {
                    entry.insert("data_len", json!(page.record.len()));
                }
                entry
            }).collect::<Vec<_>>(),
            "next_before": pages.next_before,
            "done": pages.done,
        })
    }

    /// The line: each page's record as base64 `data`.
    pub(crate) fn into_json(self) -> Value {
        let engine = &base64::engine::general_purpose::STANDARD;
        let mut value = self.meta(false);
        if let Some(entries) = value.get_mut("pages").and_then(Value::as_array_mut) {
            for (entry, page) in entries.iter_mut().zip(&self.pages.pages) {
                entry.insert("data", json!(engine.encode(&page.record)));
            }
        }
        value
    }

    /// The binary body: the line's meta with each page's `data_len`, then
    /// the page records raw, in page order.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "terminal-history answers on frames once it carries `payload: binary`"
        )
    )]
    pub(crate) fn into_body(self) -> Result<Vec<u8>, crate::binary_body::BinaryBodyError> {
        let records: Vec<&[u8]> =
            self.pages.pages.iter().map(|page| page.record.as_slice()).collect();
        crate::binary_body::encode(&self.meta(true), &records)
    }
}

pub(crate) fn read_range(mux: &Arc<Mux>, params: TerminalReadRangeParams) -> anyhow::Result<Value> {
    let surface = get_surface(mux, params.surface)?;
    require_pty(&surface)?;
    let max_bytes = params.max_bytes.map_or(TERMINAL_HISTORY_DEFAULT_BYTES, wire_usize);
    anyhow::ensure!(
        (1..=TERMINAL_HISTORY_MAX_BYTES).contains(&max_bytes),
        "invalid: max_bytes must be 1..={TERMINAL_HISTORY_MAX_BYTES}"
    );
    let vt = match params.format.as_deref().unwrap_or("text") {
        "text" => false,
        "vt" => true,
        other => anyhow::bail!("invalid: format must be text or vt, got {other:?}"),
    };
    let text = surface
        .read_marker_range(
            Some(params.marker_epoch),
            (params.from.row_marker, params.from.col),
            (params.to.row_marker, params.to.col),
            vt,
        )
        .map_err(marker_error)?;
    let (text, truncated) = truncate_text(text, max_bytes);
    Ok(json!({"surface": params.surface, "text": text, "truncated": truncated}))
}

#[cfg(test)]
mod tests {
    use super::{HistoryReply, truncate_text};
    use base64::Engine as _;
    use serde_json::{Value, json};

    fn reply(records: &[Vec<u8>]) -> HistoryReply {
        let pages = records
            .iter()
            .enumerate()
            .map(|(index, record)| ghostty_vt::HistoryPage {
                marker: 100 - index as u64,
                rows: 24,
                record: record.clone(),
            })
            .collect();
        HistoryReply::new(
            json!(7),
            ghostty_vt::HistoryPages { marker_epoch: 3, pages, next_before: Some(90), done: false },
        )
    }

    /// p05-ipc-149: a `terminal-history` binary body holds each page record
    /// raw (no base64) and stands for the line byte for byte.
    #[test]
    fn terminal_history_binary_body_equals_the_line() {
        let fixtures: [Vec<Vec<u8>>; 3] = [
            vec![],
            vec![b"GHOSTSNP one".to_vec()],
            vec![(0..=255u8).collect(), Vec::new(), vec![0xFF; 70_000]],
        ];
        for records in fixtures {
            let line = reply(&records).into_json();
            let body = reply(&records).into_body().unwrap();
            let mut read = crate::binary_body::BinaryBody::decode(&body).unwrap();
            let mut pages = read.meta.remove("pages").unwrap();
            for (entry, record) in pages.as_array_mut().unwrap().iter_mut().zip(&records) {
                let entry = entry.as_object_mut().unwrap();
                let len = crate::binary_body::take_len(entry, "data_len").unwrap();
                let raw = read.take_bytes(len).unwrap();
                assert_eq!(raw, record.as_slice());
                let data = base64::engine::general_purpose::STANDARD.encode(raw);
                entry.insert("data".into(), json!(data));
            }
            let mut rebuilt = read.finish().unwrap();
            rebuilt.insert("pages".into(), pages);
            let rebuilt = Value::Object(rebuilt);
            assert_eq!(rebuilt, line);
            assert_eq!(serde_json::to_vec(&rebuilt).unwrap(), serde_json::to_vec(&line).unwrap());
            for record in records.iter().filter(|record| record.len() >= 3) {
                let encoded = base64::engine::general_purpose::STANDARD.encode(record);
                let found = body.windows(encoded.len()).any(|window| window == encoded.as_bytes());
                assert!(!found, "a page record went into the body as base64");
            }
        }
    }

    #[test]
    fn read_range_text_is_cut_at_a_character_boundary() {
        assert_eq!(truncate_text("abc".into(), 8), ("abc".to_string(), false));
        assert_eq!(truncate_text("abcdef".into(), 4), ("abcd".to_string(), true));
        // "é" is two bytes: a cut inside it backs off to the boundary.
        assert_eq!(truncate_text("aé".into(), 2), ("a".to_string(), true));
    }
}
