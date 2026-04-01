//! Per-table counters for monitoring trace throughput and drops.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

/// Per-table counters.
pub(crate) struct TableCounters {
    pub written: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
    /// The last dropped count at which a warning was emitted.
    pub last_warned_dropped: AtomicU64,
}

impl TableCounters {
    pub fn new() -> Self {
        Self {
            written: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_warned_dropped: AtomicU64::new(0),
        }
    }
}

/// Configuration for drop warning thresholds, passed into the tracer.
pub(crate) struct DropWarnConfig {
    /// Emit a warning every N drops.
    pub warn_every_dropped: u64,
    /// Emit a warning at least once per interval.
    pub warn_interval: std::time::Duration,
    /// Timestamp of the last drop warning (any table).
    pub last_warn_time: std::sync::Mutex<Instant>,
}

impl DropWarnConfig {
    pub fn new(warn_every_dropped: u64, warn_interval_secs: u64) -> Self {
        Self {
            warn_every_dropped,
            warn_interval: std::time::Duration::from_secs(warn_interval_secs),
            last_warn_time: std::sync::Mutex::new(Instant::now()),
        }
    }

    /// Returns true if a warning should be emitted for this table's drop.
    pub fn should_warn(&self, table: &str, counters: &TableCounters) -> bool {
        let dropped = counters.dropped.load(Ordering::Relaxed);
        let last_warned = counters.last_warned_dropped.load(Ordering::Relaxed);

        // First drop for this table
        if dropped == 1 {
            counters.last_warned_dropped.store(1, Ordering::Relaxed);
            return true;
        }

        // Every N drops
        if dropped - last_warned >= self.warn_every_dropped {
            counters
                .last_warned_dropped
                .store(dropped, Ordering::Relaxed);
            return true;
        }

        // Time-based: at least once per interval
        if let Ok(mut last) = self.last_warn_time.lock() {
            if last.elapsed() >= self.warn_interval {
                *last = Instant::now();
                counters
                    .last_warned_dropped
                    .store(dropped, Ordering::Relaxed);
                return true;
            }
        }

        let _ = table;
        false
    }
}

/// A snapshot of tracer statistics.
#[derive(Clone, Debug, Default)]
pub struct TracerStats {
    /// Per-table counts of (written, dropped, errors).
    pub tables: HashMap<String, (u64, u64, u64)>,
}
