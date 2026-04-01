# Zebra Trace Events — What They Actually Mean

All events are written to DuckDB by the `zebra-trace` crate when tracing is enabled.
Each event carries an auto-generated `ts` (timestamp) column.

---

## Peer Protocol: `peer_message`

Every Zcash protocol message crossing the wire gets a `peer_message` row.

| Field | Meaning |
|-------|---------|
| `direction` | `"in"` = we received it, `"out"` = we sent it |
| `command` | Protocol command name: `inv`, `tx`, `block`, `getdata`, `getblocks`, etc. |
| `peer_addr` | The remote peer's address label |
| `message_bytes` | Wire size including the 24-byte Zcash header (NULL if unavailable) |
| `block_hash` | Present when the message carries exactly one block hash |
| `block_height` | Present when the message is a full block with a known coinbase height |

There are multiple emission points:

- **General inbound** (`connection.rs` `handle_message_as_request`): fires for every unsolicited inbound message.
- **Block response** (`connection.rs` handler match on `Message::Block`): fires when we receive a block we specifically requested via `getblocks`. Includes `block_hash` and `block_height`.
- **FindBlocks inv response** (`connection.rs` handler match on `FindBlocks` + `Message::Inv`): fires when a peer replies to our `findblocks` with an `inv` list.
- **Outbound** (`peer_tx.rs` `PeerTx::send`): fires after every successfully sent outbound message.

**Use this table to**: measure bandwidth, see message rates per peer, detect slow or noisy peers, correlate protocol exchanges.

---

## Block Lifecycle

### `block_advertised`

A peer sent us an `inv` containing a single block hash.

| Field | Meaning |
|-------|---------|
| `hash` | The block hash (hex) |
| `peer_addr` | The peer that advertised it |

**Where**: `connection.rs`, when the inbound message is `inv` with exactly one `InventoryHash::Block`.

**Means**: a peer is telling us "I have this block." We haven't fetched or verified it yet.

### `block_verified`

A block passed full consensus verification and was committed to state.

| Field | Meaning |
|-------|---------|
| `hash` | The block hash (hex) |
| `height` | The block height |
| `peer_addr` | The peer we downloaded it from (NULL for locally-submitted blocks) |
| `size_bytes` | Serialized block size |

**Where**: `sync.rs` `handle_block_response`, after the state service accepts the block.

**Means**: the block is now part of our chain state. This is the definitive "block accepted" signal.

### `block_gossiped`

We advertised a block hash to peers via `inv`.

| Field | Meaning |
|-------|---------|
| `hash` | The block hash (hex) |
| `height` | The block height |
| `is_mined` | `true` if this came from `submitblock` (local mining/submission), `false` if relayed from network |

**Where**: `sync/gossip.rs` `gossip_block`, just before calling `broadcast_network`.

**Means**: we are telling our peers "we have this block." The block was already verified and committed before this point.

### Block lifecycle order

```
block_advertised  →  (we fetch it)  →  block_verified  →  block_gossiped
   peer tells us         getdata          state accepts      we tell peers
```

---

## Transaction Lifecycle

This is where it gets subtle. The tx traces record **local** mempool events on **this** node. They do NOT tell you whether a remote peer actually received or accepted the transaction.

### `tx_pushed`

A peer sent us an unsolicited `tx` message containing a full transaction.

| Field | Meaning |
|-------|---------|
| `hash` | The transaction hash (hex) |
| `peer_addr` | The peer that sent it |

**Where**: `connection.rs`, when the inbound message is `Message::Tx`.

**Means**: a peer pushed a full transaction to us. This is the entry point — the transaction hasn't been verified yet.

### `tx_verified`

The transaction passed consensus verification.

| Field | Meaning |
|-------|---------|
| `hash` | The transaction hash (hex) |

**Where**: `mempool.rs` `poll_ready`, when a verification future resolves to `Ok`.

**Means**: the transaction is structurally valid and passes consensus rules. But it has NOT been inserted into the mempool yet — insertion can still fail (e.g., mempool full, duplicate, conflict with another tx).

### `tx_added_to_mempool`

The transaction was successfully inserted into mempool storage.

| Field | Meaning |
|-------|---------|
| `hash` | The transaction hash (hex) |

**Where**: `mempool.rs` `poll_ready`, after `storage.insert()` returns `Ok`.

**Means**: the transaction is now in our mempool and eligible for mining/gossip. This is the definitive "transaction accepted" signal for this node.

### `tx_rejected`

The transaction was rejected, either during verification or during mempool insertion.

| Field | Meaning |
|-------|---------|
| `hash` | The transaction hash (hex) |
| `reason` | Human-readable rejection reason |

**Where**: `mempool.rs` `poll_ready`, in two places:
1. When verification fails (`TransactionDownloadVerifyError`) — the tx was invalid.
2. When `storage.insert()` returns `Err` — the tx was valid but couldn't be stored (mempool full, duplicate, UTXO conflict, etc.).

**Means**: the transaction will not enter our mempool. Check `reason` to distinguish verification failure from storage rejection.

### `tx_gossiped`

We broadcast transaction IDs to peers via `inv`.

| Field | Meaning |
|-------|---------|
| `tx_count` | Total number of txids we're advertising |
| `txids` | Up to 32 of the txids (hex) |
| `truncated` | `true` if there were more than 32 txids |

**Where**: `mempool/gossip.rs` `gossip_mempool_transactions`, just before calling `AdvertiseTransactionIds`.

**Means**: we are telling our peers "we have these transactions" by sending `inv` messages with txids. Peers may then request the full transactions via `getdata`.

### Transaction lifecycle order

```
tx_pushed  →  tx_verified  →  tx_added_to_mempool  →  tx_gossiped
 peer sends     consensus       storage accepts        we tell peers
 us the tx      checks pass     the tx                 about it
           ↘                ↘
         tx_rejected      tx_rejected
         (invalid)        (storage full, dup, etc.)
```

---

## What `tx_gossiped` Does NOT Tell You

`tx_gossiped` means **we sent `inv` messages to our peers**. It does NOT mean:

- Any peer actually received the `inv` (network could drop it)
- Any peer requested the full transaction via `getdata`
- Any peer verified or accepted the transaction
- The transaction propagated beyond our immediate peers

**To verify propagation to a remote node**, look for these signals on that node for the same txid:

1. `tx_pushed` — the remote node received the full transaction from us (or another peer)
2. `tx_verified` — the remote node verified it
3. `tx_added_to_mempool` — it actually entered the remote node's mempool

`tx_gossiped` alone only proves local intent to broadcast, not network propagation.

---

## Querying Tips

```sql
-- Full tx lifecycle for a specific transaction
SELECT * FROM tx_pushed WHERE hash = '...'
UNION ALL SELECT *, NULL as peer_addr FROM tx_verified WHERE hash = '...'
UNION ALL SELECT *, NULL as peer_addr FROM tx_added_to_mempool WHERE hash = '...'
UNION ALL SELECT *, NULL as peer_addr FROM tx_rejected WHERE hash = '...'
ORDER BY ts;

-- Block sync rate (blocks verified per minute)
SELECT date_trunc('minute', ts) AS minute, count(*) AS blocks
FROM block_verified
GROUP BY 1 ORDER BY 1;

-- Bandwidth by peer
SELECT peer_addr, sum(message_bytes) AS total_bytes, count(*) AS msg_count
FROM peer_message
GROUP BY 1 ORDER BY 2 DESC;

-- Rejection reasons
SELECT reason, count(*) FROM tx_rejected GROUP BY 1 ORDER BY 2 DESC;
```
