//! Background writer thread that receives trace events and writes JSONL files.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use crossbeam_channel::Receiver;

use crate::{counters::TableCounters, envelope::Envelope};

/// An erased event ready for writing.
///
/// The closure produces the serialized JSON bytes for an `Envelope<T>`.
pub(crate) struct ErasedEvent {
    pub table: &'static str,
    pub serialize_fn: Box<dyn FnOnce(&WriterContext) -> Result<Vec<u8>, serde_json::Error> + Send>,
}

/// Commands sent from the [`TracerHandle`] to the writer thread.
pub(crate) enum WriterCommand {
    /// Drain remaining events, flush all files, and exit.
    Stop,
}

/// Context available to the serialization closure.
pub(crate) struct WriterContext {
    pub run_id: String,
    pub node_id: String,
    pub seq: AtomicU64,
    pub epoch: Instant,
}

impl WriterContext {
    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn monotonic_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    pub fn wall_clock_ts(&self) -> String {
        humantime::format_rfc3339_millis(SystemTime::now()).to_string()
    }

    pub fn make_envelope<T: serde::Serialize>(&self, table: &'static str, msg: T) -> Envelope<T> {
        Envelope {
            run_id: self.run_id.clone(),
            node_id: self.node_id.clone(),
            table,
            seq: self.next_seq(),
            ts: self.wall_clock_ts(),
            monotonic_ns: self.monotonic_ns(),
            msg,
        }
    }
}

/// Spawn the background writer thread.
///
/// Returns a `JoinHandle` that can be used to wait for shutdown.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_writer(
    trace_dir: PathBuf,
    peer_rx: Receiver<ErasedEvent>,
    event_rx: Receiver<ErasedEvent>,
    control_rx: Receiver<WriterCommand>,
    counters: Arc<HashMap<String, TableCounters>>,
    run_id: u128,
    node_id: String,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("zebra-trace-writer".into())
        .spawn(move || {
            writer_loop(
                trace_dir, peer_rx, event_rx, control_rx, counters, run_id, node_id,
            );
        })
        .expect("failed to spawn zebra-trace writer thread")
}

/// The flush interval for buffered writers.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Number of writes before forcing a flush.
const WRITES_BEFORE_FLUSH: usize = 1024;

/// Per-table writer state tracking.
struct TableWriter {
    writer: BufWriter<File>,
    /// If true, this table has been disabled due to repeated I/O errors.
    disabled: bool,
    /// Count of consecutive I/O errors for this table.
    consecutive_errors: u32,
}

/// Number of consecutive I/O errors before disabling a table.
const MAX_CONSECUTIVE_IO_ERRORS: u32 = 10;

fn writer_loop(
    trace_dir: PathBuf,
    peer_rx: Receiver<ErasedEvent>,
    event_rx: Receiver<ErasedEvent>,
    control_rx: Receiver<WriterCommand>,
    counters: Arc<HashMap<String, TableCounters>>,
    run_id: u128,
    node_id: String,
) {
    let ctx = WriterContext {
        run_id: format!("{run_id:032x}"),
        node_id,
        seq: AtomicU64::new(0),
        epoch: Instant::now(),
    };

    let mut writers: HashMap<&'static str, TableWriter> = HashMap::new();
    let mut writes_since_flush: usize = 0;
    let mut last_flush = Instant::now();

    loop {
        // Use select! to drain data channels and check control channel
        crossbeam_channel::select! {
            recv(control_rx) -> msg => {
                if msg.is_ok() {
                    // Stop command received: drain remaining events then exit
                    drain_channel(&peer_rx, &ctx, &mut writers, &counters, &trace_dir);
                    drain_channel(&event_rx, &ctx, &mut writers, &counters, &trace_dir);
                    flush_all(&mut writers);
                    return;
                }
            },
            recv(peer_rx) -> msg => {
                if let Ok(event) = msg {
                    write_event(event, &ctx, &mut writers, &counters, &trace_dir);
                    writes_since_flush += 1;
                }
            },
            recv(event_rx) -> msg => {
                if let Ok(event) = msg {
                    write_event(event, &ctx, &mut writers, &counters, &trace_dir);
                    writes_since_flush += 1;
                }
            },
            default(FLUSH_INTERVAL) => {
                // Periodic flush
                flush_all(&mut writers);
                last_flush = Instant::now();
                writes_since_flush = 0;
                continue;
            },
        }

        // Flush periodically
        if writes_since_flush >= WRITES_BEFORE_FLUSH || last_flush.elapsed() >= FLUSH_INTERVAL {
            flush_all(&mut writers);
            last_flush = Instant::now();
            writes_since_flush = 0;
        }
    }
}

fn drain_channel(
    rx: &Receiver<ErasedEvent>,
    ctx: &WriterContext,
    writers: &mut HashMap<&'static str, TableWriter>,
    counters: &Arc<HashMap<String, TableCounters>>,
    trace_dir: &std::path::Path,
) {
    while let Ok(event) = rx.try_recv() {
        write_event(event, ctx, writers, counters, trace_dir);
    }
}

fn write_event(
    event: ErasedEvent,
    ctx: &WriterContext,
    writers: &mut HashMap<&'static str, TableWriter>,
    counters: &Arc<HashMap<String, TableCounters>>,
    trace_dir: &std::path::Path,
) {
    let table = event.table;

    // Check if this table is disabled
    if let Some(tw) = writers.get(table) {
        if tw.disabled {
            return;
        }
    }

    // Serialize the event
    let json_bytes = match (event.serialize_fn)(ctx) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(?e, table, "trace event serialization failed, event dropped");
            if let Some(c) = counters.get(table) {
                c.errors.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
    };

    // Get or create the writer for this table
    let tw = if let Some(tw) = writers.get_mut(table) {
        tw
    } else {
        let path = trace_dir.join(format!("{table}.jsonl"));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                writers.insert(
                    table,
                    TableWriter {
                        writer: BufWriter::new(file),
                        disabled: false,
                        consecutive_errors: 0,
                    },
                );
                writers.get_mut(table).expect("just inserted")
            }
            Err(e) => {
                tracing::error!(?e, ?path, "failed to open trace file, disabling table");
                writers.insert(
                    table,
                    TableWriter {
                        writer: BufWriter::new(File::open("/dev/null").unwrap()),
                        disabled: true,
                        consecutive_errors: 0,
                    },
                );
                if let Some(c) = counters.get(table) {
                    c.errors.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
        }
    };

    if tw.disabled {
        return;
    }

    // Write the JSON line
    let write_result = tw
        .writer
        .write_all(&json_bytes)
        .and_then(|_| tw.writer.write_all(b"\n"));

    match write_result {
        Ok(()) => {
            tw.consecutive_errors = 0;
            if let Some(c) = counters.get(table) {
                c.written.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(e) => {
            tw.consecutive_errors += 1;
            if let Some(c) = counters.get(table) {
                c.errors.fetch_add(1, Ordering::Relaxed);
            }
            if tw.consecutive_errors >= MAX_CONSECUTIVE_IO_ERRORS {
                tracing::error!(
                    ?e,
                    table,
                    consecutive_errors = tw.consecutive_errors,
                    "disabling trace table after repeated I/O failures"
                );
                tw.disabled = true;
            } else {
                tracing::warn!(?e, table, "failed to write trace event");
            }
        }
    }
}

fn flush_all(writers: &mut HashMap<&'static str, TableWriter>) {
    for (table, tw) in writers.iter_mut() {
        if tw.disabled {
            continue;
        }
        if let Err(e) = tw.writer.flush() {
            tw.consecutive_errors += 1;
            if tw.consecutive_errors >= MAX_CONSECUTIVE_IO_ERRORS {
                tracing::error!(
                    ?e,
                    table,
                    "disabling trace table after repeated flush failures"
                );
                tw.disabled = true;
            } else {
                tracing::warn!(?e, table, "failed to flush trace file");
            }
        }
    }
}
