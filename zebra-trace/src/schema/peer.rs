//! Peer message trace events.

use serde::Serialize;

use crate::TraceEvent;

/// A peer protocol message was sent or received.
#[derive(Serialize)]
pub struct PeerMessage {
    /// "in" for inbound, "out" for outbound.
    pub direction: String,
    /// The Zcash protocol command name (e.g. "inv", "tx", "block").
    pub command: String,
    /// The peer address label (metrics_label from ConnectedAddr).
    pub peer_addr: String,
    /// The exact message size on the wire, including the 24-byte Zcash message header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_bytes: Option<usize>,
    /// The block hash carried by this message, if it is block-related and singular.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<String>,
    /// The block height carried by this message, if it is a full block and the height is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_height: Option<u32>,
}

impl TraceEvent for PeerMessage {
    fn table() -> &'static str {
        "peer_message"
    }
}
