//! Block-related trace events.

use serde::Serialize;

use crate::TraceEvent;

/// A block hash was advertised by a peer (via `inv`).
#[derive(Serialize)]
pub struct BlockAdvertised {
    /// The block hash as a hex string.
    pub hash: String,
    /// The peer that advertised the block.
    pub peer_addr: String,
}

impl TraceEvent for BlockAdvertised {
    fn table() -> &'static str {
        "block_advertised"
    }
}

/// A block hash was gossiped to peers.
#[derive(Serialize)]
pub struct BlockGossiped {
    /// The block hash as a hex string.
    pub hash: String,
    /// The block height.
    pub height: u32,
    /// Whether this block originated from local block submission or mining.
    pub is_mined: bool,
}

impl TraceEvent for BlockGossiped {
    fn table() -> &'static str {
        "block_gossiped"
    }
}

/// A block was successfully verified.
#[derive(Serialize)]
pub struct BlockVerified {
    /// The block hash as a hex string.
    pub hash: String,
    /// The block height.
    pub height: u32,
    /// The peer that supplied this block, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_addr: Option<String>,
    /// The serialized size of the block in bytes, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<usize>,
}

impl TraceEvent for BlockVerified {
    fn table() -> &'static str {
        "block_verified"
    }
}
