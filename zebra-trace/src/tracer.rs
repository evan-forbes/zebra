//! The main `Tracer` handle, clonable and cheaply shareable.

use std::{
    collections::{HashMap, HashSet},
    sync::{atomic::Ordering, Arc},
    thread,
};

use crossbeam_channel::Sender;

use crate::{
    config::TracerConfig,
    counters::{DropWarnConfig, TableCounters, TracerStats},
    writer::{self, ErasedEvent, WriterCommand},
    TraceEvent,
};

/// The shared state held by all producer clones.
struct SharedState {
    /// Sender for peer message events (high-volume).
    peer_tx: Sender<ErasedEvent>,
    /// Sender for general events.
    event_tx: Sender<ErasedEvent>,
    /// Per-table counters.
    counters: Arc<HashMap<String, TableCounters>>,
    /// Tables we are collecting. Empty means all.
    enabled_tables: HashSet<String>,
    /// Drop warning configuration.
    drop_warn: DropWarnConfig,
}

/// A structured JSONL tracer.
///
/// `Clone` is cheap (Arc). When the inner is `None`, all methods are
/// zero-cost noops (the macro short-circuits on `is_collecting()`).
///
/// Call [`Tracer::stop()`] on the owner to flush and shut down the writer
/// thread. Producer clones are producer-only — they do not own the join handle.
#[derive(Clone)]
pub struct Tracer {
    shared: Option<Arc<SharedState>>,
}

/// An owned handle to the writer thread, returned alongside the `Tracer`
/// from [`Tracer::from_config()`].
///
/// Call [`TracerHandle::stop()`] to send a shutdown signal, wait for
/// acknowledgment, and join the writer thread. This works regardless
/// of how many `Tracer` clones still exist.
pub struct TracerHandle {
    /// Control channel to send Stop to the writer thread.
    control_tx: Option<Sender<WriterCommand>>,
    /// The writer thread join handle.
    writer_handle: Option<thread::JoinHandle<()>>,
    /// Shared counters for shutdown summary.
    counters: Arc<HashMap<String, TableCounters>>,
}

impl std::fmt::Debug for Tracer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracer")
            .field("active", &self.shared.is_some())
            .finish()
    }
}

impl Tracer {
    /// Create a noop tracer. All methods are zero-cost.
    pub fn noop() -> Self {
        Self { shared: None }
    }

    /// Create a tracer from configuration.
    ///
    /// Returns a `(Tracer, TracerHandle)` pair. The `Tracer` is cheaply
    /// clonable and used by producers. The `TracerHandle` owns the writer
    /// thread and must be used to stop the tracer on shutdown.
    ///
    /// Returns `(noop tracer, noop handle)` if `config.enabled` is false.
    pub fn from_config(config: &TracerConfig) -> Result<(Self, TracerHandle), std::io::Error> {
        if !config.enabled {
            return Ok((
                Self::noop(),
                TracerHandle {
                    control_tx: None,
                    writer_handle: None,
                    counters: Arc::new(HashMap::new()),
                },
            ));
        }

        // Validate configuration
        config
            .validate()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        // Pre-create trace directory (fail fast if unusable)
        std::fs::create_dir_all(&config.trace_dir)?;

        let run_id: u128 = rand::random();

        // Build known tables list for counters
        let known_tables: &[&str] = &[
            "peer_message",
            "block_advertised",
            "block_gossiped",
            "block_verified",
            "tx_pushed",
            "tx_verified",
            "tx_added_to_mempool",
            "tx_gossiped",
            "tx_rejected",
        ];

        let mut counters = HashMap::new();
        for &table in known_tables {
            counters.insert(table.to_string(), TableCounters::new());
        }
        let counters = Arc::new(counters);

        let enabled_tables: HashSet<String> = config.tables.iter().cloned().collect();

        let (peer_tx, peer_rx) = crossbeam_channel::bounded(config.peer_message_queue_size);
        let (event_tx, event_rx) = crossbeam_channel::bounded(config.event_queue_size);
        let (control_tx, control_rx) = crossbeam_channel::bounded(1);

        let writer_handle = writer::spawn_writer(
            config.trace_dir.clone(),
            peer_rx,
            event_rx,
            control_rx,
            counters.clone(),
            run_id,
            config.node_id.clone(),
        );

        // Log startup info for operator visibility
        let tables_desc = if config.tables.is_empty() {
            "all".to_string()
        } else {
            config.tables.join(", ")
        };
        tracing::info!(
            trace_dir = ?config.trace_dir,
            tables = %tables_desc,
            peer_message_queue_size = config.peer_message_queue_size,
            event_queue_size = config.event_queue_size,
            "structured tracer initialized"
        );

        let tracer = Self {
            shared: Some(Arc::new(SharedState {
                peer_tx,
                event_tx,
                counters: counters.clone(),
                enabled_tables,
                drop_warn: DropWarnConfig::new(
                    config.warn_every_dropped,
                    config.warn_interval_secs,
                ),
            })),
        };

        let handle = TracerHandle {
            control_tx: Some(control_tx),
            writer_handle: Some(writer_handle),
            counters,
        };

        Ok((tracer, handle))
    }

    /// Returns true if this tracer is actively collecting events.
    #[inline]
    pub fn is_collecting(&self) -> bool {
        self.shared.is_some()
    }

    /// Returns true if this tracer is collecting events for the given table.
    #[inline]
    pub fn is_table_enabled(&self, table: &str) -> bool {
        match &self.shared {
            None => false,
            Some(inner) => inner.enabled_tables.is_empty() || inner.enabled_tables.contains(table),
        }
    }

    /// Write a trace event, constructing it lazily.
    ///
    /// The closure is only called if tracing is enabled for this table.
    /// Uses `try_send` to avoid blocking — events are dropped if the queue is full.
    pub fn write_lazy<T, F>(&self, f: F)
    where
        T: TraceEvent,
        F: FnOnce() -> T,
    {
        let inner = match &self.shared {
            Some(inner) => inner,
            None => return,
        };

        let table = T::table();
        if !inner.enabled_tables.is_empty() && !inner.enabled_tables.contains(table) {
            return;
        }

        let event = f();
        let erased = ErasedEvent {
            table,
            serialize_fn: Box::new(move |ctx| {
                let envelope = ctx.make_envelope(table, event);
                serde_json::to_vec(&envelope)
            }),
        };

        // Use the peer channel for peer_message (high-volume), otherwise the event channel
        let sender = if table == "peer_message" {
            &inner.peer_tx
        } else {
            &inner.event_tx
        };

        if sender.try_send(erased).is_err() {
            if let Some(c) = inner.counters.get(table) {
                c.dropped.fetch_add(1, Ordering::Relaxed);
                if inner.drop_warn.should_warn(table, c) {
                    let dropped = c.dropped.load(Ordering::Relaxed);
                    tracing::warn!(table, dropped, "trace events dropped (queue full)");
                }
            }
        }
    }

    /// Get a snapshot of tracer statistics.
    pub fn snapshot(&self) -> TracerStats {
        let inner = match &self.shared {
            Some(inner) => inner,
            None => return TracerStats::default(),
        };

        let mut stats = TracerStats::default();
        for (table, counters) in inner.counters.iter() {
            stats.tables.insert(
                table.clone(),
                (
                    counters.written.load(Ordering::Relaxed),
                    counters.dropped.load(Ordering::Relaxed),
                    counters.errors.load(Ordering::Relaxed),
                ),
            );
        }
        stats
    }
}

impl TracerHandle {
    /// Stop the tracer, flushing remaining events and joining the writer thread.
    ///
    /// Sends a `Stop` command to the writer, waits for it to drain and flush,
    /// then joins the thread. This works regardless of how many `Tracer` clones
    /// still exist.
    ///
    /// Emits a shutdown summary with per-table counters.
    pub fn stop(&mut self) {
        // Send stop command
        if let Some(control_tx) = self.control_tx.take() {
            let _ = control_tx.send(WriterCommand::Stop);
        }

        // Join the writer thread
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join();
        }

        // Emit shutdown summary
        let mut total_written = 0u64;
        let mut total_dropped = 0u64;
        let mut total_errors = 0u64;

        for (table, c) in self.counters.iter() {
            let written = c.written.load(Ordering::Relaxed);
            let dropped = c.dropped.load(Ordering::Relaxed);
            let errors = c.errors.load(Ordering::Relaxed);

            if written > 0 || dropped > 0 || errors > 0 {
                tracing::info!(
                    table,
                    written,
                    dropped,
                    errors,
                    "trace table shutdown summary"
                );
            }

            total_written += written;
            total_dropped += dropped;
            total_errors += errors;
        }

        tracing::info!(
            total_written,
            total_dropped,
            total_errors,
            "structured tracer shutdown complete"
        );
    }
}
