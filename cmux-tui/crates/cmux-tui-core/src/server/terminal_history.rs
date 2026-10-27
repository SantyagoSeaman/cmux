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
use crate::binary_body::BulkReply;
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

/// Most pages one reply holds (the schema's `max_items` of `pages`); a
/// longer answer ends early with `next_before`, as one past `max_bytes` does.
pub(crate) const TERMINAL_HISTORY_MAX_PAGES: usize = 1024;

pub(crate) fn history(
    mux: &Arc<Mux>,
    params: TerminalHistoryParams,
) -> anyhow::Result<HistoryReply> {
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
    Ok(HistoryReply::new(json!(params.surface), cap_pages(pages, TERMINAL_HISTORY_MAX_PAGES)))
}

/// `pages` cut to the first `max` pages; a cut answer is not done and
/// continues before its last page, as one cut at `max_bytes` does.
fn cap_pages(mut pages: ghostty_vt::HistoryPages, max: usize) -> ghostty_vt::HistoryPages {
    if pages.pages.len() > max {
        pages.pages.truncate(max);
        pages.next_before = pages.pages.last().map(|page| page.marker);
        pages.done = false;
    }
    pages
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
}

impl BulkReply for HistoryReply {
    /// Each page's record as base64 `data`.
    fn into_json(self) -> Value {
        let engine = &base64::engine::general_purpose::STANDARD;
        let mut value = self.meta(false);
        if let Some(entries) = value.get_mut("pages").and_then(Value::as_array_mut) {
            for (entry, page) in entries.iter_mut().zip(&self.pages.pages) {
                entry.insert("data", json!(engine.encode(&page.record)));
            }
        }
        value
    }

    /// Each page's `data_len`, then the page records raw, in page order.
    fn body_parts(&self) -> (Value, Vec<&[u8]>) {
        let records = self.pages.pages.iter().map(|page| page.record.as_slice()).collect();
        (self.meta(true), records)
    }
}

impl HistoryReply {
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
    use super::{HistoryReply, cap_pages, truncate_text};
    use crate::binary_body::{BulkReply as _, body_of};
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
            let body = body_of(&reply(&records));
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

    /// The schema bounds `pages` (`max_items`): a longer answer is cut and
    /// continues before its last kept page.
    #[test]
    fn terminal_history_pages_are_cut_at_the_schema_bound() {
        let records = vec![b"p".to_vec(); 5];
        let HistoryReply { pages, .. } = reply(&records);
        let cut = cap_pages(pages, 3);
        assert_eq!(cut.pages.iter().map(|page| page.marker).collect::<Vec<_>>(), [100, 99, 98]);
        assert_eq!((cut.next_before, cut.done), (Some(98), false));
        let HistoryReply { pages, .. } = reply(&records);
        let whole = cap_pages(pages, 5);
        assert_eq!((whole.pages.len(), whole.next_before, whole.done), (5, Some(90), false));
    }

    #[test]
    fn read_range_text_is_cut_at_a_character_boundary() {
        assert_eq!(truncate_text("abc".into(), 8), ("abc".to_string(), false));
        assert_eq!(truncate_text("abcdef".into(), 4), ("abcd".to_string(), true));
        // "é" is two bytes: a cut inside it backs off to the boundary.
        assert_eq!(truncate_text("aé".into(), 2), ("a".to_string(), true));
    }
}
