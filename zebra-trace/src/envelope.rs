//! The envelope wraps every trace event with metadata.

use serde::Serialize;

/// A wrapper around a trace event that adds metadata.
///
/// Every line in a JSONL file is an `Envelope` containing the event
/// plus a run ID, node ID, table name, sequence number, and timestamps.
#[derive(Serialize)]
pub(crate) struct Envelope<T: Serialize> {
    /// Unique ID for this Zebra process run (hex-encoded u128).
    pub run_id: String,
    /// User-configured node identifier.
    pub node_id: String,
    /// The table (event type) name.
    pub table: &'static str,
    /// Monotonically increasing sequence number per run.
    pub seq: u64,
    /// Wall-clock timestamp as RFC 3339.
    pub ts: String,
    /// Monotonic nanoseconds since an arbitrary epoch (for ordering).
    pub monotonic_ns: u64,
    /// The actual event payload.
    pub msg: T,
}
