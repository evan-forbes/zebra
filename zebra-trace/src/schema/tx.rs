//! Transaction-related trace events.

use serde::Serialize;

use crate::TraceEvent;

/// A transaction was pushed by a peer.
#[derive(Serialize)]
pub struct TxPushed {
    /// The transaction hash as a hex string.
    pub hash: String,
    /// The peer that pushed the transaction.
    pub peer_addr: String,
}

impl TraceEvent for TxPushed {
    fn table() -> &'static str {
        "tx_pushed"
    }
}

/// A transaction was successfully verified.
#[derive(Serialize)]
pub struct TxVerified {
    /// The transaction hash as a hex string.
    pub hash: String,
}

impl TraceEvent for TxVerified {
    fn table() -> &'static str {
        "tx_verified"
    }
}

/// A transaction was added to the mempool.
#[derive(Serialize)]
pub struct TxAddedToMempool {
    /// The transaction hash as a hex string.
    pub hash: String,
}

impl TraceEvent for TxAddedToMempool {
    fn table() -> &'static str {
        "tx_added_to_mempool"
    }
}

/// Transaction IDs were gossiped to peers.
#[derive(Serialize)]
pub struct TxGossiped {
    /// Total number of transaction IDs gossiped.
    pub tx_count: usize,
    /// Transaction IDs included in this event (capped to `max_txids_per_event`).
    pub txids: Vec<String>,
    /// Whether the txids list was truncated.
    pub truncated: bool,
}

impl TraceEvent for TxGossiped {
    fn table() -> &'static str {
        "tx_gossiped"
    }
}

/// A transaction was rejected from the mempool.
#[derive(Serialize)]
pub struct TxRejected {
    /// The transaction hash as a hex string.
    pub hash: String,
    /// The rejection reason.
    pub reason: String,
}

impl TraceEvent for TxRejected {
    fn table() -> &'static str {
        "tx_rejected"
    }
}
