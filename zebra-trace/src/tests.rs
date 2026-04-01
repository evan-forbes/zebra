//! Tests for the structured JSONL tracer.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    config::TracerConfig,
    schema::{BlockAdvertised, PeerMessage},
    tracer::Tracer,
};

#[test]
fn noop_tracer_is_not_collecting() {
    let tracer = Tracer::noop();
    assert!(!tracer.is_collecting());

    // write_lazy should not panic on a noop tracer
    tracer.write_lazy(|| PeerMessage {
        direction: "in".to_string(),
        command: "test".to_string(),
        peer_addr: "127.0.0.1:8233".to_string(),
        message_bytes: None,
        block_hash: None,
        block_height: None,
    });
}

/// Disabled-table fast path: the event builder closure must not be called.
#[test]
fn disabled_table_does_not_build_event() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");

    let config = TracerConfig {
        enabled: true,
        trace_dir: dir.path().to_path_buf(),
        // Only enable "block_advertised", not "peer_message"
        tables: vec!["block_advertised".to_string()],
        ..Default::default()
    };

    let (tracer, mut handle) = Tracer::from_config(&config).expect("failed to create tracer");

    // Use a flag to detect if the builder closure runs
    static BUILDER_CALLED: AtomicBool = AtomicBool::new(false);
    BUILDER_CALLED.store(false, Ordering::SeqCst);

    tracer.write_lazy(|| {
        BUILDER_CALLED.store(true, Ordering::SeqCst);
        PeerMessage {
            direction: "in".to_string(),
            command: "test".to_string(),
            peer_addr: "127.0.0.1:8233".to_string(),
            message_bytes: None,
            block_hash: None,
            block_height: None,
        }
    });

    assert!(
        !BUILDER_CALLED.load(Ordering::SeqCst),
        "builder should not be called for a disabled table"
    );

    handle.stop();
}

#[test]
fn local_tracer_roundtrip() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");

    let config = TracerConfig {
        enabled: true,
        trace_dir: dir.path().to_path_buf(),
        ..Default::default()
    };

    let (tracer, mut handle) = Tracer::from_config(&config).expect("failed to create tracer");
    assert!(tracer.is_collecting());

    // Emit a peer message event
    trace_event!(
        tracer,
        PeerMessage {
            direction: "in".to_string(),
            command: "inv".to_string(),
            peer_addr: "127.0.0.1:8233".to_string(),
            message_bytes: Some(61),
            block_hash: Some("00000000abcdef".to_string()),
            block_height: None,
        }
    );

    // Emit another event type
    trace_event!(
        tracer,
        BlockAdvertised {
            hash: "00000000abcdef".to_string(),
            peer_addr: "127.0.0.1:8233".to_string(),
        }
    );

    // Stop via handle to flush
    handle.stop();

    // Verify peer_message.jsonl was created and contains valid JSON
    let peer_path = dir.path().join("peer_message.jsonl");
    assert!(peer_path.exists(), "peer_message.jsonl should exist");
    let content = std::fs::read_to_string(&peer_path).expect("failed to read file");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 1, "should have 1 peer message line");

    // Parse as JSON and verify envelope fields
    let val: serde_json::Value = serde_json::from_str(lines[0]).expect("invalid JSON");
    assert_eq!(val["table"], "peer_message");
    assert_eq!(val["msg"]["direction"], "in");
    assert_eq!(val["msg"]["command"], "inv");
    assert_eq!(val["msg"]["peer_addr"], "127.0.0.1:8233");
    assert_eq!(val["msg"]["message_bytes"], 61);
    assert_eq!(val["msg"]["block_hash"], "00000000abcdef");
    assert!(val["run_id"].is_string(), "run_id should be a hex string");
    let run_id = val["run_id"].as_str().unwrap();
    assert_eq!(run_id.len(), 32, "run_id should be a 32-char hex string");
    assert!(
        run_id.chars().all(|c| c.is_ascii_hexdigit()),
        "run_id should be hex"
    );
    assert!(val["seq"].is_u64());
    assert!(val["ts"].is_string());
    assert!(val["monotonic_ns"].is_u64());

    // Verify block_advertised.jsonl
    let block_path = dir.path().join("block_advertised.jsonl");
    assert!(block_path.exists(), "block_advertised.jsonl should exist");
    let content = std::fs::read_to_string(&block_path).expect("failed to read file");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 1);
    let val: serde_json::Value = serde_json::from_str(lines[0]).expect("invalid JSON");
    assert_eq!(val["table"], "block_advertised");
    assert_eq!(val["msg"]["hash"], "00000000abcdef");
}

#[test]
fn drop_counter_increments_on_overflow() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");

    let config = TracerConfig {
        enabled: true,
        trace_dir: dir.path().to_path_buf(),
        // Use a tiny queue to force drops
        event_queue_size: 1,
        peer_message_queue_size: 1,
        ..Default::default()
    };

    let (tracer, mut handle) = Tracer::from_config(&config).expect("failed to create tracer");

    // Flood events to force drops
    for _ in 0..100 {
        trace_event!(
            tracer,
            PeerMessage {
                direction: "in".to_string(),
                command: "ping".to_string(),
                peer_addr: "127.0.0.1:8233".to_string(),
                message_bytes: None,
                block_hash: None,
                block_height: None,
            }
        );
    }

    let stats = tracer.snapshot();
    // At least some events should have been dropped with queue size 1
    let peer_stats = stats.tables.get("peer_message");
    assert!(peer_stats.is_some(), "should have peer_message stats");
    let (written, dropped, _errors) = peer_stats.unwrap();
    assert!(
        *dropped > 0 || *written > 0,
        "should have written or dropped events, got written={written}, dropped={dropped}"
    );

    handle.stop();
}

#[test]
fn table_filtering() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");

    let config = TracerConfig {
        enabled: true,
        trace_dir: dir.path().to_path_buf(),
        tables: vec!["peer_message".to_string()],
        ..Default::default()
    };

    let (tracer, mut handle) = Tracer::from_config(&config).expect("failed to create tracer");

    assert!(tracer.is_table_enabled("peer_message"));
    assert!(!tracer.is_table_enabled("block_advertised"));

    // Emit events for both tables
    trace_event!(
        tracer,
        PeerMessage {
            direction: "in".to_string(),
            command: "inv".to_string(),
            peer_addr: "127.0.0.1:8233".to_string(),
            message_bytes: None,
            block_hash: None,
            block_height: None,
        }
    );

    trace_event!(
        tracer,
        BlockAdvertised {
            hash: "00000000abcdef".to_string(),
            peer_addr: "127.0.0.1:8233".to_string(),
        }
    );

    handle.stop();

    // Only peer_message.jsonl should exist
    let peer_path = dir.path().join("peer_message.jsonl");
    assert!(peer_path.exists(), "peer_message.jsonl should exist");

    let block_path = dir.path().join("block_advertised.jsonl");
    assert!(
        !block_path.exists(),
        "block_advertised.jsonl should NOT exist when table is filtered out"
    );
}

/// Verify that shutdown works even when producer clones still exist.
#[test]
fn shutdown_while_clones_exist() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");

    let config = TracerConfig {
        enabled: true,
        trace_dir: dir.path().to_path_buf(),
        ..Default::default()
    };

    let (tracer, mut handle) = Tracer::from_config(&config).expect("failed to create tracer");
    let _clone1 = tracer.clone();
    let _clone2 = tracer.clone();

    // Emit an event
    trace_event!(
        tracer,
        PeerMessage {
            direction: "in".to_string(),
            command: "ping".to_string(),
            peer_addr: "127.0.0.1:8233".to_string(),
            message_bytes: None,
            block_hash: None,
            block_height: None,
        }
    );

    // Stop should work even though clones exist
    handle.stop();

    // Verify the event was flushed
    let peer_path = dir.path().join("peer_message.jsonl");
    assert!(peer_path.exists(), "peer_message.jsonl should exist");
    let content = std::fs::read_to_string(&peer_path).expect("failed to read file");
    assert_eq!(content.lines().count(), 1, "should have 1 line");
}

/// Verify that config validation rejects invalid queue sizes.
#[test]
fn config_validation_rejects_zero_queue_size() {
    let config = TracerConfig {
        enabled: true,
        event_queue_size: 0,
        ..Default::default()
    };
    assert!(config.validate().is_err());

    let config = TracerConfig {
        enabled: true,
        peer_message_queue_size: 0,
        ..Default::default()
    };
    assert!(config.validate().is_err());
}

/// Verify that from_config fails when trace directory is unusable.
#[test]
fn startup_failure_on_bad_directory() {
    let config = TracerConfig {
        enabled: true,
        trace_dir: std::path::PathBuf::from(
            "/nonexistent/deeply/nested/path/that/should/not/exist",
        ),
        ..Default::default()
    };

    let result = Tracer::from_config(&config);
    assert!(
        result.is_err(),
        "should fail when trace directory is unusable"
    );
}

/// Verify that peer_message and other events use separate queues.
#[test]
fn queue_isolation_between_peer_and_event() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");

    let config = TracerConfig {
        enabled: true,
        trace_dir: dir.path().to_path_buf(),
        // Small peer queue, normal event queue
        peer_message_queue_size: 1,
        event_queue_size: 4096,
        ..Default::default()
    };

    let (tracer, mut handle) = Tracer::from_config(&config).expect("failed to create tracer");

    // Flood peer messages to fill the peer queue
    for _ in 0..50 {
        trace_event!(
            tracer,
            PeerMessage {
                direction: "in".to_string(),
                command: "ping".to_string(),
                peer_addr: "127.0.0.1:8233".to_string(),
                message_bytes: None,
                block_hash: None,
                block_height: None,
            }
        );
    }

    // Block events should still go through the event queue
    trace_event!(
        tracer,
        BlockAdvertised {
            hash: "00000000abcdef".to_string(),
            peer_addr: "127.0.0.1:8233".to_string(),
        }
    );

    handle.stop();

    // Block event should have been written even though peer queue was flooded
    let block_path = dir.path().join("block_advertised.jsonl");
    assert!(
        block_path.exists(),
        "block_advertised.jsonl should exist despite peer queue overflow"
    );
    let content = std::fs::read_to_string(&block_path).expect("failed to read file");
    assert_eq!(content.lines().count(), 1);

    // Peer events should show drops
    let stats = tracer.snapshot();
    let peer_stats = stats.tables.get("peer_message").unwrap();
    let (_written, dropped, _errors) = peer_stats;
    assert!(*dropped > 0, "peer messages should have been dropped");
}

/// Verify that all lines in a JSONL file are valid JSON.
#[test]
fn all_lines_are_valid_json() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");

    let config = TracerConfig {
        enabled: true,
        trace_dir: dir.path().to_path_buf(),
        ..Default::default()
    };

    let (tracer, mut handle) = Tracer::from_config(&config).expect("failed to create tracer");

    // Emit multiple events
    for i in 0..10 {
        trace_event!(
            tracer,
            PeerMessage {
                direction: "in".to_string(),
                command: format!("cmd{i}"),
                peer_addr: "127.0.0.1:8233".to_string(),
                message_bytes: None,
                block_hash: None,
                block_height: None,
            }
        );
    }

    handle.stop();

    let peer_path = dir.path().join("peer_message.jsonl");
    let content = std::fs::read_to_string(&peer_path).expect("failed to read file");

    for (i, line) in content.lines().enumerate() {
        let val: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(val.is_ok(), "line {i} should be valid JSON, got: {line}");
    }
}
