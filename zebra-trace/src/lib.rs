//! Structured JSONL tracing for Zebra.
//!
//! Each trace point writes a single JSON line to a table-specific file.
//! The system is zero-cost when disabled (noop tracer) and best-effort
//! low-overhead when enabled (bounded non-blocking queues, buffered I/O).

mod config;
mod counters;
mod envelope;
pub mod schema;
mod tracer;
mod writer;

pub use config::TracerConfig;
pub use counters::TracerStats;
pub use tracer::{Tracer, TracerHandle};

/// Trait for events that can be written to a trace file.
///
/// Each event type maps to a specific table (file).
pub trait TraceEvent: serde::Serialize + Send + 'static {
    /// The table name for this event type. Used as the JSONL filename.
    fn table() -> &'static str;
}

/// Emit a trace event if the tracer is collecting.
///
/// Uses struct literal syntax — field names are checked at compile time,
/// and reordering fields cannot silently corrupt logs.
///
/// Short-circuits immediately if tracing is disabled (noop tracer).
///
/// # Examples
///
/// ```ignore
/// trace_event!(tracer, PeerMessage {
///     direction: "in".to_string(),
///     command: msg.command().to_string(),
///     peer_addr: self.metrics_label.clone(),
///     message_bytes: Some(msg.wire_size()),
///     block_hash: msg.single_block_hash().map(|hash| hash.to_string()),
///     block_height: msg.block_height(),
/// });
/// ```
#[macro_export]
macro_rules! trace_event {
    ($tracer:expr, $($event_path:ident)::+ { $($field:ident : $val:expr),* $(,)? }) => {
        if $tracer.is_collecting() {
            $tracer.write_lazy(|| { $($event_path)::+ { $($field : $val),* } });
        }
    };
}

#[cfg(test)]
mod tests;
