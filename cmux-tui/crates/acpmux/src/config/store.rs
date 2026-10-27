//! `store` in config.json: where sessions and their event logs are kept.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum StoreMode {
    Memory,
    #[default]
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StoreConfig {
    #[serde(default)]
    pub mode: StoreMode,
    /// Segment roll size in bytes for the event log.
    #[serde(default = "default_segment_bytes")]
    pub segment_bytes: u64,
}

fn default_segment_bytes() -> u64 {
    8 * 1024 * 1024
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self { mode: StoreMode::Local, segment_bytes: default_segment_bytes() }
    }
}
