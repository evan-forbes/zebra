//! Helpers for generating a local Testnet chain with deterministic funded keys.

use std::{collections::HashSet, sync::Arc};

use chrono::{DateTime, Utc};
use ripemd::Ripemd160;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    amount::{Amount, NonNegative},
    block::{
        self, merkle::AuthDataRoot, Block, ChainHistoryBlockTxAuthCommitmentHash, Header, Height,
        CHAIN_HISTORY_ACTIVATION_RESERVED, ZCASH_BLOCK_VERSION,
    },
    history_tree::{HistoryTree, HistoryTreeError},
    orchard,
    parameters::{
        self,
        constants::magics,
        subsidy::SubsidyError,
        testnet::{ConfiguredActivationHeights, ConfiguredCheckpoints},
        Magic, Network, NetworkKind, NetworkUpgrade, GENESIS_PREVIOUS_BLOCK_HASH,
    },
    sapling,
    serialization::ZcashSerialize,
    transaction::{LockTime, Transaction},
    transparent,
    work::{
        difficulty::{ParameterDifficulty as _, U256},
        equihash::Solution,
    },
};

/// Options for generating a local Testnet chain artifact.
#[derive(Clone, Debug)]
pub struct LocalTestnetGenesisOptions {
    /// The configured testnet name.
    pub network_name: String,
    /// Optional network magic. If `None`, a deterministic magic is derived from `network_name`.
    pub network_magic: Option<Magic>,
    /// Activate this upgrade at [`Self::latest_upgrade_height`].
    ///
    /// Supported values are `Nu5`, `Nu6`, and `Nu6_1`.
    pub latest_network_upgrade: NetworkUpgrade,
    /// Activation height for all upgrades through `latest_network_upgrade`.
    ///
    /// Must be above height 0.
    pub latest_upgrade_height: Height,
    /// Number of blocks to generate after genesis.
    ///
    /// A value of `100` means generated chain heights are `0..=100`.
    pub premined_block_count: u32,
    /// Block time used for genesis.
    pub start_time: DateTime<Utc>,
    /// If true, generated network parameters disable PoW checks.
    ///
    /// This generator currently only supports `true`.
    pub disable_pow: bool,
}

impl Default for LocalTestnetGenesisOptions {
    fn default() -> Self {
        Self {
            network_name: "LocalGenesisNet".to_string(),
            network_magic: None,
            latest_network_upgrade: NetworkUpgrade::Nu6_1,
            latest_upgrade_height: Height(1),
            premined_block_count: transparent::MIN_TRANSPARENT_COINBASE_MATURITY,
            start_time: DateTime::from_timestamp(1_700_000_000, 0)
                .expect("valid local genesis timestamp"),
            disable_pow: true,
        }
    }
}

/// Deterministic funded transparent key material derived from a user-provided name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FundedKeyMaterial {
    /// The input name used to derive this key.
    pub name: String,
    /// The secp256k1 private key bytes encoded in lowercase hex.
    pub secret_key_hex: String,
    /// The compressed secp256k1 public key encoded in lowercase hex.
    pub public_key_hex: String,
    /// Transparent P2PKH address for this key on Testnet/Regtest address format.
    pub address: transparent::Address,
}

/// Generated local Testnet artifact.
#[derive(Clone, Debug)]
pub struct GeneratedLocalTestnet {
    /// Configured Testnet network parameters that match the generated blocks.
    pub network: Network,
    /// Deterministic funded keys, in input order.
    pub funded_keys: Vec<FundedKeyMaterial>,
    /// Generated chain blocks, including genesis at index 0.
    pub blocks: Vec<Arc<Block>>,
    /// Generated checkpoints matching [`Self::blocks`].
    pub checkpoints: Vec<(Height, block::Hash)>,
    /// History tree state at the tip of the generated chain.
    history_tree: HistoryTree,
    /// Sapling note commitment tree root (stays empty for generated chains).
    sapling_root: sapling::tree::Root,
    /// Orchard note commitment tree tracking all Orchard actions in the chain.
    orchard_tree: orchard::tree::NoteCommitmentTree,
    /// Options used to generate this chain, needed for rebuilding network params.
    options: LocalTestnetGenesisOptions,
}

impl GeneratedLocalTestnet {
    /// Returns the generated genesis block.
    pub fn genesis_block(&self) -> Arc<Block> {
        self.blocks
            .first()
            .expect("generated chain always includes genesis")
            .clone()
    }

    /// Returns the generated genesis block in hex-encoded serialized form.
    pub fn genesis_hex(&self) -> Result<String, std::io::Error> {
        let mut bytes = Vec::new();
        self.genesis_block().zcash_serialize(&mut bytes)?;
        Ok(hex::encode(bytes))
    }

    /// Returns the current Orchard note commitment tree root.
    pub fn orchard_tree_root(&self) -> orchard::tree::Root {
        self.orchard_tree.root()
    }

    /// Returns the funded scripts derived from the funded keys.
    pub fn funded_scripts(&self) -> Vec<transparent::Script> {
        self.funded_keys
            .iter()
            .map(|k| k.address.script())
            .collect()
    }

    /// Appends a new block containing a coinbase transaction and `extra_txs`.
    ///
    /// The coinbase distributes the block subsidy among the funded keys.
    /// Orchard note commitments from `extra_txs` are added to the internal
    /// Orchard tree so that [`Self::orchard_tree_root`] reflects the new state.
    ///
    /// Returns the height of the new block.
    pub fn append_block(
        &mut self,
        extra_txs: Vec<Arc<Transaction>>,
    ) -> Result<Height, LocalGenesisError> {
        let height = Height(
            self.blocks
                .len()
                .try_into()
                .expect("block count fits in u32"),
        );
        let funded_scripts = self.funded_scripts();
        let difficulty_threshold = self.network.target_difficulty_limit().to_compact();

        let subsidy = parameters::subsidy::block_subsidy(height, &self.network)?;
        let outputs = distribute_subsidy(subsidy, &funded_scripts);

        let coinbase = coinbase_transaction_for_height(&self.network, height, outputs);
        let mut transactions = vec![Arc::new(coinbase)];
        transactions.extend(extra_txs);

        // Update Orchard tree with note commitments from all transactions.
        for tx in &transactions {
            for cm_x in tx.orchard_note_commitments() {
                self.orchard_tree
                    .append(*cm_x)
                    .map_err(|e| LocalGenesisError::Parameters(e.to_string()))?;
            }
        }
        let orchard_root = self.orchard_tree.root();

        let merkle_root = transactions.iter().collect();

        let last_block = self
            .blocks
            .last()
            .expect("generated chain always includes genesis");
        let previous_hash = last_block.hash();
        let previous_time = last_block.header.time;
        let time = previous_time + NetworkUpgrade::target_spacing_for_height(&self.network, height);

        let commitment_bytes = block_commitment_bytes_for_height(
            &self.network,
            height,
            &self.history_tree,
            &transactions,
            &self.sapling_root,
        )?;

        let header = Header {
            version: ZCASH_BLOCK_VERSION,
            previous_block_hash: previous_hash,
            merkle_root,
            commitment_bytes: commitment_bytes.into(),
            time,
            difficulty_threshold,
            nonce: [0; 32].into(),
            solution: Solution::for_proposal(),
        };

        let block = Arc::new(Block {
            header: Arc::new(header),
            transactions,
        });

        let block_hash = block.hash();
        self.checkpoints.push((height, block_hash));

        self.history_tree.push(
            &self.network,
            block.clone(),
            &self.sapling_root,
            &orchard_root,
        )?;
        self.blocks.push(block);

        Ok(height)
    }

    /// Rebuilds the network parameters with updated checkpoints.
    ///
    /// Call this after [`Self::append_block`] to ensure nodes loading the
    /// payload validate the extended chain correctly.
    pub fn rebuild_network(&mut self) -> Result<(), LocalGenesisError> {
        self.network = build_network_parameters(
            &self.options,
            self.blocks[0].hash(),
            self.checkpoints.clone(),
        )?;
        Ok(())
    }
}

/// Errors for local testnet genesis generation.
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum LocalGenesisError {
    #[error("at least one non-empty funded key name is required")]
    EmptyNames,

    #[error("found duplicate funded key name: {0}")]
    DuplicateName(String),

    #[error("funded key names must not be empty")]
    EmptyName,

    #[error("latest upgrade height must be greater than 0")]
    InvalidLatestUpgradeHeight,

    #[error("unsupported latest network upgrade for this tool: {0:?}")]
    UnsupportedLatestUpgrade(NetworkUpgrade),

    #[error("this generator currently supports disable_pow=true only")]
    UnsupportedPowMode,

    #[error("missing history tree root while building commitment at height {0:?}")]
    MissingHistoryTree(Height),

    #[error("failed to build configured testnet parameters: {0}")]
    Parameters(String),

    #[error(transparent)]
    Subsidy(#[from] SubsidyError),

    #[error(transparent)]
    HistoryTree(#[from] HistoryTreeError),
}

/// Generate a configured local testnet chain and deterministic funded keys from `names`.
pub fn generate_local_testnet_with_funded_keys<I, S>(
    names: I,
    options: LocalTestnetGenesisOptions,
) -> Result<GeneratedLocalTestnet, LocalGenesisError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if !options.disable_pow {
        return Err(LocalGenesisError::UnsupportedPowMode);
    }

    if options.latest_upgrade_height == Height(0) {
        return Err(LocalGenesisError::InvalidLatestUpgradeHeight);
    }

    let mut seen_names = HashSet::new();
    let mut ordered_names = Vec::new();

    for raw_name in names {
        let name = raw_name.as_ref().trim().to_string();
        if name.is_empty() {
            return Err(LocalGenesisError::EmptyName);
        }
        if !seen_names.insert(name.clone()) {
            return Err(LocalGenesisError::DuplicateName(name));
        }
        ordered_names.push(name);
    }

    if ordered_names.is_empty() {
        return Err(LocalGenesisError::EmptyNames);
    }

    let funded_keys: Vec<_> = ordered_names
        .iter()
        .map(|name| funded_key_from_name(name, NetworkKind::Testnet))
        .collect();
    let funded_scripts: Vec<_> = funded_keys.iter().map(|key| key.address.script()).collect();

    let placeholder_genesis_hash = block::Hash([1; 32]);
    let placeholder_checkpoints = vec![(Height(0), placeholder_genesis_hash)];

    let prototype_network =
        build_network_parameters(&options, placeholder_genesis_hash, placeholder_checkpoints)?;

    let mut blocks = Vec::with_capacity((options.premined_block_count as usize) + 1);
    let mut checkpoints = Vec::with_capacity((options.premined_block_count as usize) + 1);

    let mut previous_hash = GENESIS_PREVIOUS_BLOCK_HASH;
    let mut previous_time = options.start_time;
    let difficulty_threshold = prototype_network.target_difficulty_limit().to_compact();

    let sapling_tree = sapling::tree::NoteCommitmentTree::default();
    let orchard_tree = orchard::tree::NoteCommitmentTree::default();
    let sapling_root = sapling_tree.root();
    let orchard_root = orchard_tree.root();
    let mut history_tree = HistoryTree::default();

    for height in (0..=options.premined_block_count).map(Height) {
        let subsidy = parameters::subsidy::block_subsidy(height, &prototype_network)?;
        let outputs = distribute_subsidy(subsidy, &funded_scripts);

        let coinbase = coinbase_transaction_for_height(&prototype_network, height, outputs);
        let transactions = vec![Arc::new(coinbase)];
        let merkle_root = transactions.iter().collect();

        let time = if height == Height(0) {
            options.start_time
        } else {
            previous_time + NetworkUpgrade::target_spacing_for_height(&prototype_network, height)
        };

        let commitment_bytes = block_commitment_bytes_for_height(
            &prototype_network,
            height,
            &history_tree,
            &transactions,
            &sapling_root,
        )?;

        let header = Header {
            version: ZCASH_BLOCK_VERSION,
            previous_block_hash: previous_hash,
            merkle_root,
            commitment_bytes: commitment_bytes.into(),
            time,
            difficulty_threshold,
            nonce: [0; 32].into(),
            solution: Solution::for_proposal(),
        };

        let block = Arc::new(Block {
            header: Arc::new(header),
            transactions,
        });

        previous_hash = block.hash();
        previous_time = block.header.time;
        checkpoints.push((height, previous_hash));

        history_tree.push(
            &prototype_network,
            block.clone(),
            &sapling_root,
            &orchard_root,
        )?;
        blocks.push(block);
    }

    let final_network = build_network_parameters(&options, blocks[0].hash(), checkpoints.clone())?;

    Ok(GeneratedLocalTestnet {
        network: final_network,
        funded_keys,
        blocks,
        checkpoints,
        history_tree,
        sapling_root,
        orchard_tree,
        options,
    })
}

/// Generate with [`LocalTestnetGenesisOptions::default`].
pub fn generate_local_testnet<I, S>(names: I) -> Result<GeneratedLocalTestnet, LocalGenesisError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    generate_local_testnet_with_funded_keys(names, LocalTestnetGenesisOptions::default())
}

fn build_network_parameters(
    options: &LocalTestnetGenesisOptions,
    genesis_hash: block::Hash,
    checkpoints: Vec<(Height, block::Hash)>,
) -> Result<Network, LocalGenesisError> {
    let network_magic = options
        .network_magic
        .unwrap_or_else(|| derive_network_magic(&options.network_name));
    let activation_heights = activation_heights_for_latest_upgrade(
        options.latest_network_upgrade,
        options.latest_upgrade_height,
    )?;

    let params = parameters::testnet::Parameters::build()
        .with_network_name(&options.network_name)
        .map_err(|err| LocalGenesisError::Parameters(err.to_string()))?
        .with_network_magic(network_magic)
        .map_err(|err| LocalGenesisError::Parameters(err.to_string()))?
        .with_genesis_hash(genesis_hash)
        .map_err(|err| LocalGenesisError::Parameters(err.to_string()))?
        .with_activation_heights(activation_heights)
        .map_err(|err| LocalGenesisError::Parameters(err.to_string()))?
        .with_slow_start_interval(Height::MIN)
        .clear_funding_streams()
        .with_lockbox_disbursements(Vec::new())
        // Match regtest defaults for local, low-friction block generation.
        .with_target_difficulty_limit(U256::from_big_endian(&[0x0f; 32]))
        .map_err(|err| LocalGenesisError::Parameters(err.to_string()))?
        .with_halving_interval(144)
        .map_err(|err| LocalGenesisError::Parameters(err.to_string()))?
        .with_disable_pow(options.disable_pow)
        .with_unshielded_coinbase_spends(true)
        .with_checkpoints(ConfiguredCheckpoints::HeightsAndHashes(checkpoints))
        .map_err(|err| LocalGenesisError::Parameters(err.to_string()))?
        .to_network()
        .map_err(|err| LocalGenesisError::Parameters(err.to_string()))?;

    Ok(params)
}

fn activation_heights_for_latest_upgrade(
    latest_upgrade: NetworkUpgrade,
    activation_height: Height,
) -> Result<ConfiguredActivationHeights, LocalGenesisError> {
    let height = activation_height.0;
    let mut configured = ConfiguredActivationHeights {
        overwinter: Some(height),
        sapling: Some(height),
        blossom: Some(height),
        heartwood: Some(height),
        canopy: Some(height),
        nu5: Some(height),
        ..ConfiguredActivationHeights::default()
    };

    match latest_upgrade {
        NetworkUpgrade::Nu5 => {}
        NetworkUpgrade::Nu6 => {
            configured.nu6 = Some(height);
        }
        NetworkUpgrade::Nu6_1 => {
            configured.nu6 = Some(height);
            configured.nu6_1 = Some(height);
        }
        unsupported => return Err(LocalGenesisError::UnsupportedLatestUpgrade(unsupported)),
    }

    Ok(configured)
}

fn derive_network_magic(network_name: &str) -> Magic {
    let mut counter: u32 = 0;

    loop {
        let mut hash_input = Sha256::new();
        hash_input.update(b"zebra.local.genesis.magic");
        hash_input.update(network_name.as_bytes());
        hash_input.update(counter.to_le_bytes());

        let digest = hash_input.finalize();
        let mut bytes = [0; 4];
        bytes.copy_from_slice(&digest[..4]);
        let magic = Magic(bytes);

        if magic != magics::MAINNET && magic != magics::TESTNET && magic != magics::REGTEST {
            return magic;
        }

        counter = counter.wrapping_add(1);
    }
}

fn funded_key_from_name(name: &str, network_kind: NetworkKind) -> FundedKeyMaterial {
    let secret_key = secret_key_from_name(name);
    let secp = Secp256k1::new();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let pub_key_hash = hash160(&public_key.serialize());
    let address = transparent::Address::from_pub_key_hash(network_kind, pub_key_hash);

    FundedKeyMaterial {
        name: name.to_string(),
        secret_key_hex: hex::encode(secret_key.secret_bytes()),
        public_key_hex: hex::encode(public_key.serialize()),
        address,
    }
}

fn secret_key_from_name(name: &str) -> SecretKey {
    let mut counter: u32 = 0;

    loop {
        let mut hash_input = Sha256::new();
        hash_input.update(b"zebra.local.genesis.key");
        hash_input.update(name.as_bytes());
        hash_input.update(counter.to_le_bytes());

        let digest = hash_input.finalize();
        if let Ok(secret_key) = SecretKey::from_slice(&digest) {
            return secret_key;
        }

        counter = counter.wrapping_add(1);
    }
}

fn hash160(data: &[u8]) -> [u8; 20] {
    let sha256 = Sha256::digest(data);
    let ripemd160 = Ripemd160::digest(sha256);

    let mut result = [0; 20];
    result.copy_from_slice(&ripemd160);
    result
}

fn distribute_subsidy(
    subsidy: Amount<NonNegative>,
    scripts: &[transparent::Script],
) -> Vec<(Amount<NonNegative>, transparent::Script)> {
    let recipient_count = i64::try_from(scripts.len()).expect("script count fits in i64");
    let total = subsidy.zatoshis();
    let base = total / recipient_count;
    let mut remainder = total % recipient_count;

    scripts
        .iter()
        .map(|script| {
            let mut amount = base;
            if remainder > 0 {
                amount += 1;
                remainder -= 1;
            }

            (Amount::new(amount), script.clone())
        })
        .collect()
}

fn coinbase_transaction_for_height(
    network: &Network,
    height: Height,
    outputs: Vec<(Amount<NonNegative>, transparent::Script)>,
) -> Transaction {
    if height == Height(0) {
        let regtest_genesis = crate::block::genesis::regtest_genesis_block();
        return regtest_genesis.transactions[0].as_ref().clone();
    }

    let miner_data = b"zebra-local-genesis".to_vec();

    match NetworkUpgrade::current(network, height) {
        NetworkUpgrade::Nu5 | NetworkUpgrade::Nu6 | NetworkUpgrade::Nu6_1 | NetworkUpgrade::Nu7 => {
            Transaction::new_v5_coinbase(network, height, outputs, miner_data)
        }
        NetworkUpgrade::Sapling
        | NetworkUpgrade::Blossom
        | NetworkUpgrade::Heartwood
        | NetworkUpgrade::Canopy => Transaction::new_v4_coinbase(height, outputs, miner_data),
        NetworkUpgrade::Genesis | NetworkUpgrade::BeforeOverwinter | NetworkUpgrade::Overwinter => {
            Transaction::V1 {
                inputs: vec![transparent::Input::new_coinbase(height, miner_data, None)],
                outputs: outputs
                    .into_iter()
                    .map(|(amount, script)| transparent::Output::new(amount, script))
                    .collect(),
                lock_time: LockTime::unlocked(),
            }
        }
    }
}

fn block_commitment_bytes_for_height(
    network: &Network,
    height: Height,
    history_tree: &HistoryTree,
    transactions: &[Arc<Transaction>],
    sapling_root: &sapling::tree::Root,
) -> Result<[u8; 32], LocalGenesisError> {
    let network_upgrade = NetworkUpgrade::current(network, height);
    let heartwood_activation = NetworkUpgrade::Heartwood.activation_height(network);

    let bytes = match network_upgrade {
        NetworkUpgrade::Genesis | NetworkUpgrade::BeforeOverwinter | NetworkUpgrade::Overwinter => {
            [0; 32]
        }
        NetworkUpgrade::Sapling | NetworkUpgrade::Blossom => (*sapling_root).into(),
        NetworkUpgrade::Heartwood | NetworkUpgrade::Canopy => {
            if Some(height) == heartwood_activation {
                CHAIN_HISTORY_ACTIVATION_RESERVED
            } else {
                history_tree
                    .hash()
                    .ok_or(LocalGenesisError::MissingHistoryTree(height))?
                    .into()
            }
        }
        NetworkUpgrade::Nu5 | NetworkUpgrade::Nu6 | NetworkUpgrade::Nu6_1 | NetworkUpgrade::Nu7 => {
            let history_root = history_tree
                .hash()
                .or_else(|| {
                    (Some(height) == heartwood_activation)
                        .then_some(CHAIN_HISTORY_ACTIVATION_RESERVED.into())
                })
                .ok_or(LocalGenesisError::MissingHistoryTree(height))?;

            let auth_data_root: AuthDataRoot = transactions.iter().collect();
            let block_commitment = ChainHistoryBlockTxAuthCommitmentHash::from_commitments(
                &history_root,
                &auth_data_root,
            );

            block_commitment.into()
        }
    };

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_custom_local_testnet_chain() -> Result<(), Box<dyn std::error::Error>> {
        let _init_guard = zebra_test::init();

        let options = LocalTestnetGenesisOptions {
            premined_block_count: 3,
            ..LocalTestnetGenesisOptions::default()
        };

        let generated = generate_local_testnet_with_funded_keys(["miner-1", "miner-2"], options)?;

        assert_eq!(generated.funded_keys.len(), 2);
        assert_eq!(generated.blocks.len(), 4);
        assert_eq!(generated.checkpoints.len(), 4);

        let genesis_hash = generated.genesis_block().hash();
        assert_eq!(generated.network.genesis_hash(), genesis_hash);
        assert_eq!(generated.checkpoints[0], (Height(0), genesis_hash));

        let height_1_block = &generated.blocks[1];
        assert!(matches!(
            height_1_block.commitment(&generated.network)?,
            block::Commitment::ChainHistoryBlockTxAuthCommitment(_)
        ));

        Ok(())
    }

    #[test]
    fn rejects_duplicate_names() {
        let _init_guard = zebra_test::init();

        let err = generate_local_testnet(["miner-1", "miner-1"])
            .expect_err("duplicate names should fail");
        assert!(matches!(err, LocalGenesisError::DuplicateName(_)));
    }
}
