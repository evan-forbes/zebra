# Zebra Repository Structure Summary (Feb 2026)

This document provides a high-level overview of the Zebra repository to assist with onboarding and architectural understanding.

## Core Architecture & Design Patterns

Zebra is a high-performance Zcash node implementation in Rust, leveraging the following foundational libraries and patterns:

*   **Service-Oriented Architecture:** Most components are implemented as `tower::Service` types. This provides a uniform request-response model, simplifies unit testing (via mocking), and facilitates features like backpressure, load balancing, and observability.
*   **Asynchronous Processing:** Built on `tokio`, Zebra uses asynchronous tasks for concurrency.
*   **Framework:** The `abscissa` framework provides the application structure, including configuration handling and subcommand management.
*   **Layered Design:** The repository is split into specialized crates, each focusing on a specific layer of the Zcash protocol (chain data, consensus, network, state, RPC).

## Key Components (Crates)

### 1. `zebrad` (Application Crate)
*   **Entry Point:** `zebrad/src/bin/zebrad/main.rs` (launches the `zebrad` application).
*   **Core Commands:** `StartCmd` (in `zebrad/src/commands/start.rs`) is the primary command for running the node.
*   **Application Structure:** Detailed architectural overview is provided in `zebrad/src/commands/start.rs` documentation.

### 2. `zebra-chain` (Zcash Data Structures)
*   **Purpose:** Defines the foundational types for the Zcash protocol.
*   **Key Structs:**
    *   `Block` (`zebra-chain/src/block.rs`): Represents a Zcash block, containing a header and transactions.
    *   `Transaction` (`zebra-chain/src/transaction.rs`): Represents Zcash transactions (supporting multiple versions: V1-V5, etc.).
    *   `Height` (`zebra-chain/src/block/height.rs`): Represents a block height in the chain.
    *   `Hash` (`zebra-chain/src/block/hash.rs`): Represents block and transaction hashes.

### 3. `zebra-consensus` (Verification Logic)
*   **Purpose:** Implements all Zcash consensus rules.
*   **Key Logic:**
    *   `BlockVerifierRouter` (`zebra-consensus/src/router.rs`): Routes verification requests to either the `CheckpointVerifier` (for historical blocks) or `SemanticBlockVerifier` (for newer blocks).
    *   `SemanticBlockVerifier` (`zebra-consensus/src/block.rs`): Handles full consensus validation, including Proof of Work (Equihash), timestamp checks, and transaction validation.

### 4. `zebra-network` (P2P Networking)
*   **Purpose:** Handles peer discovery, connection management, and protocol messaging.
*   **Key Logic:**
    *   `PeerSet` (`zebra-network/src/peer_set.rs`): A `tower::Service` that manages a set of peer connections and balances requests across them.
    *   `protocol::external` and `protocol::internal`: Handle the external Bitcoin-based protocol and internal Zebra request-response models.

### 5. `zebra-state` (Chain Storage)
*   **Purpose:** Manages the persistent on-disk state and the transient in-memory state.
*   **Key Logic:**
    *   `StateService` (`zebra-state/src/service.rs`): Orchestrates writes and redirects reads.
    *   `NonFinalizedState`: Manages recent blocks (approximately the last 100), handles forks, and is stored in memory.
    *   `FinalizedState`: Stores the single best chain of older blocks on disk using a database (typically RocksDB).
    *   `LatestChainTip`: A channel providing real-time updates of the current best tip.

### 6. `zebra-rpc` (Remote Procedure Calls)
*   **Purpose:** Provides a JSON-RPC interface for wallets and other tools.
*   **Key Logic:**
    *   `RpcImpl` (`zebra-rpc/src/methods.rs`): Implements Zcash-compatible RPC methods (e.g., `getblock`, `sendrawtransaction`).

## Critical Property Testing

Zebra relies heavily on property-based testing (using the `proptest` crate) to ensure consensus correctness and data integrity:

*   **Data Structures:** `zebra-chain` uses property tests to verify serialization/deserialization consistency.
*   **Consensus:** `zebra-consensus` uses property tests (e.g., in `zebra-consensus/src/transaction/tests/prop.rs`) to verify transaction and block validation rules.
*   **State Integrity:** `zebra-state` has extensive property tests (e.g., in `zebra-state/src/service/tests.rs` and `zebra-state/src/service/check/tests/nullifier.rs`) to ensure that state transitions are valid and that nullifiers are correctly tracked to prevent double-spending.

## Important Routines

*   **Chain Synchronization:** The `ChainSync` task (in `zebrad/src/components/sync.rs`) continuously queries the network for new blocks and submits them for verification.
*   **Gossip Tasks:** `BlockGossip` and `TransactionGossip` tasks (in `zebrad/src/commands/start.rs`) distribute new blocks and unmined transactions to the network.
*   **Mempool Management:** The `Mempool` service (in `zebrad/src/components/mempool.rs`) manages unmined transactions.

This structure allows Zebra to be modular, testable, and robust, providing a clean implementation of the Zcash protocol.
