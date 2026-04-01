//! Configuration for structured JSONL tracing.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration for the structured JSONL tracer.
///
/// When `enabled` is false, a noop tracer is created (zero-cost).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct TracerConfig {
    /// Whether structured tracing is enabled.
    pub enabled: bool,

    /// Directory where JSONL trace files are written.
    /// Defaults to `./traces/`.
    pub trace_dir: PathBuf,

    /// If non-empty, only these tables will be traced.
    /// An empty list means all tables are traced.
    pub tables: Vec<String>,

    /// Capacity of the peer message queue (high-volume).
    pub peer_message_queue_size: usize,

    /// Capacity of the general event queue.
    pub event_queue_size: usize,

    /// A unique identifier for this node, included in every envelope.
    /// Defaults to empty string.
    pub node_id: String,

    /// Emit a drop warning every N dropped events per table.
    /// Defaults to 1000.
    pub warn_every_dropped: u64,

    /// Emit a drop warning at least once every this many seconds while
    /// drops continue, even if the count threshold hasn't been reached.
    /// Defaults to 30.
    pub warn_interval_secs: u64,

    /// Maximum number of transaction IDs to include in a `TxGossiped` event.
    /// Excess IDs are truncated and a `truncated` flag is set.
    /// Defaults to 32.
    pub max_txids_per_event: usize,
}

impl Default for TracerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trace_dir: PathBuf::from("traces"),
            tables: Vec::new(),
            peer_message_queue_size: 16_384,
            event_queue_size: 4_096,
            node_id: String::new(),
            warn_every_dropped: 1_000,
            warn_interval_secs: 30,
            max_txids_per_event: 32,
        }
    }
}

impl TracerConfig {
    /// Validate the configuration, returning an error if settings are invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.peer_message_queue_size == 0 {
            return Err("peer_message_queue_size must be > 0".into());
        }
        if self.event_queue_size == 0 {
            return Err("event_queue_size must be > 0".into());
        }
        if self.max_txids_per_event == 0 {
            return Err("max_txids_per_event must be > 0".into());
        }
        Ok(())
    }
}
