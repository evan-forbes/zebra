// Disabled due to warnings in criterion macros.
#![allow(missing_docs)]
// Benchmark metadata is printed in machine-readable lines before each case runs.
#![allow(clippy::print_stdout)]

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    future::Future,
    io::Cursor,
    pin::Pin,
    sync::{Arc, Once},
    task::{Context, Poll},
    time::Duration,
};

use chrono::{DateTime, Utc};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use futures::{stream::FuturesUnordered, StreamExt};
use tokio::sync::oneshot;
use tower::{buffer::Buffer, util::BoxService, Service, ServiceExt};

use zebra_chain::{
    block::{Block, Height, MAX_BLOCK_BYTES},
    parameters::Network,
    serialization::{DateTime32, ZcashDeserialize, ZcashSerialize},
    transaction::Transaction,
    transparent,
};
use zebra_consensus::{
    error::TransactionError,
    transaction::{self as tx, Request},
    BoxError,
};
use zebra_node_services::mempool;
use zebra_state as zs;
use zebra_test::vectors::MAINNET_BLOCKS;

const ALLOW_TRANSPARENT_PREVOUTS_WITHOUT_UTXOS: bool = false;
const SHIELDED_POOL_COUNT: usize = 3;
const SHIELDED_POOLS: [ShieldedPool; SHIELDED_POOL_COUNT] = [
    ShieldedPool::Sapling,
    ShieldedPool::Orchard,
    ShieldedPool::Sprout,
];
const MAINNET_BLOCK_HEADER_BYTES: usize = 1_487;
const WORKLOAD_SELECTION_STRATEGY: &str = "exact_action_mix_max_actions_under_max_block_bytes";

const BENCHMARK_CASES: &[BenchmarkCase] = &[
    BenchmarkCase {
        name: "available_orchard_heavy",
        rayon_threads: 4,
        tokio_worker_threads: 4,
        action_mix: ShieldedActionMix::new(0, 100, 0),
    },
    BenchmarkCase {
        name: "available_sapling_heavy",
        rayon_threads: 4,
        tokio_worker_threads: 4,
        action_mix: ShieldedActionMix::new(100, 0, 0),
    },
    BenchmarkCase {
        name: "available_sapling_orchard_even",
        rayon_threads: 4,
        tokio_worker_threads: 4,
        action_mix: ShieldedActionMix::new(50, 50, 0),
    },
];

#[derive(Clone, Debug)]
struct BenchmarkCase {
    name: &'static str,
    rayon_threads: usize,
    tokio_worker_threads: usize,
    action_mix: ShieldedActionMix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShieldedPool {
    Sapling,
    Orchard,
    Sprout,
}

#[derive(Clone, Copy, Debug)]
struct ShieldedActionMix {
    percents: [u8; SHIELDED_POOL_COUNT],
}

#[derive(Clone, Debug)]
struct CandidateTx {
    transaction: Arc<Transaction>,
    serialized_len: usize,
    height: Height,
    time: DateTime<Utc>,
    counts: ActionCounts,
}

#[derive(Clone, Debug)]
struct Workload {
    requests: Vec<Request>,
    target_action_counts: ShieldedActionCounts,
    stats: WorkloadStats,
}

#[derive(Clone, Debug, Default)]
struct WorkloadStats {
    modeled_block_bytes: usize,
    serialized_bytes: usize,
    unique_transactions: usize,
    repeated_transactions: usize,
    action_counts: ActionCounts,
    verifier_checks: VerifierCheckCounts,
}

#[derive(Clone, Copy, Debug, Default)]
struct ActionCounts {
    transparent_inputs: usize,
    sapling_spends: usize,
    sapling_outputs: usize,
    orchard_actions: usize,
    sprout_joinsplits: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct ShieldedActionCounts {
    counts: [usize; SHIELDED_POOL_COUNT],
}

#[derive(Clone, Copy, Debug, Default)]
struct VerifierCheckCounts {
    sapling_bundles: usize,
    orchard_bundles: usize,
    sprout_joinsplit_proofs: usize,
    sprout_signatures: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct CandidateLoadStats {
    skipped_coinbase: usize,
    skipped_unsupported_version: usize,
    skipped_transparent_prevouts: usize,
}

type BenchmarkMempool =
    Buffer<BoxService<mempool::Request, mempool::Response, BoxError>, mempool::Request>;

type TxVerifier = Buffer<BoxService<Request, tx::Response, TransactionError>, Request>;

#[derive(Clone, Debug)]
struct BenchmarkState;

impl Service<zs::Request> for BenchmarkState {
    type Response = zs::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: zs::Request) -> Self::Future {
        Box::pin(async move {
            match request {
                zs::Request::BestChainNextMedianTimePast => {
                    Ok(zs::Response::BestChainNextMedianTimePast(DateTime32::MIN))
                }
                zs::Request::CheckBestChainTipNullifiersAndAnchors(_) => {
                    Ok(zs::Response::ValidBestChainTipNullifiersAndAnchors)
                }
                unexpected => Err(format!(
                    "unexpected state request in tx verifier benchmark: {unexpected:?}"
                )
                .into()),
            }
        })
    }
}

fn worst_case_tx_verification(c: &mut Criterion) {
    let first_case = validate_benchmark_cases();
    init_rayon(first_case.rayon_threads);

    let (candidates, load_stats) = load_mainnet_candidates();
    println!(
        "worst_case_tx_verification: loaded {} mainnet candidate txs; skipped {} coinbase, {} unsupported-version, {} transparent-prevout txs",
        candidates.len(),
        load_stats.skipped_coinbase,
        load_stats.skipped_unsupported_version,
        load_stats.skipped_transparent_prevouts,
    );
    println!(
        "worst_case_tx_verification: mode=tx verifier repeated workload; max_block_bytes={}; limitation=uses repeated mainnet tx vectors, not a consensus-valid synthetic block",
        max_block_bytes(),
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(first_case.tokio_worker_threads)
        .enable_all()
        .build()
        .expect("tokio runtime should build for the benchmark");

    for case in BENCHMARK_CASES {
        let Some(workload) = build_workload(case, &candidates) else {
            println!(
                "worst_case_tx_verification: skipping case {}; no repeated mainnet candidate workload fit the requested shielded action mix under the max block size",
                case.name,
            );
            continue;
        };

        print_workload_metadata(case, &workload);

        c.bench_with_input(
            BenchmarkId::new("tx_verifier_repeated_workload", case.name),
            &workload.requests,
            |b, requests| {
                b.iter(|| {
                    let verified = runtime.block_on(async {
                        let verifier = make_transaction_verifier(requests.len().saturating_add(1));
                        verify_requests(verifier, requests).await
                    });
                    black_box(verified);
                });
            },
        );
    }
}

fn validate_benchmark_cases() -> &'static BenchmarkCase {
    let first_case = BENCHMARK_CASES
        .first()
        .expect("at least one benchmark case is configured");

    for case in BENCHMARK_CASES {
        assert_eq!(
            case.rayon_threads, first_case.rayon_threads,
            "Rayon's global thread pool can only be configured once per process; use one rayon_threads value per benchmark run"
        );
        assert_eq!(
            case.tokio_worker_threads, first_case.tokio_worker_threads,
            "global proof verifier workers are tied to the Tokio runtime; use one tokio_worker_threads value per benchmark run"
        );

        assert_eq!(
            case.action_mix.total_percent(),
            100,
            "case {} must allocate exactly 100 percent of shielded pool actions",
            case.name,
        );
    }

    first_case
}

fn init_rayon(threads: usize) {
    static INIT_RAYON: Once = Once::new();

    INIT_RAYON.call_once(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .expect("rayon global thread pool should be initialized before proof verification");
    });
}

fn load_mainnet_candidates() -> (Vec<CandidateTx>, CandidateLoadStats) {
    let mut candidates = Vec::new();
    let mut stats = CandidateLoadStats::default();

    for (&height, &block_bytes) in MAINNET_BLOCKS.iter() {
        let block = Block::zcash_deserialize(Cursor::new(block_bytes))
            .expect("mainnet block test vector should deserialize");
        assert_eq!(
            block
                .header
                .zcash_serialize_to_vec()
                .expect("mainnet block header test vector should serialize")
                .len(),
            MAINNET_BLOCK_HEADER_BYTES,
            "benchmark block-size accounting should match mainnet serialized block headers",
        );

        for transaction in block.transactions {
            if transaction.is_coinbase() {
                stats.skipped_coinbase += 1;
                continue;
            }

            if transaction.version() < 4 {
                stats.skipped_unsupported_version += 1;
                continue;
            }

            let counts = ActionCounts::from_transaction(&transaction);

            if !ALLOW_TRANSPARENT_PREVOUTS_WITHOUT_UTXOS && counts.transparent_inputs > 0 {
                stats.skipped_transparent_prevouts += 1;
                continue;
            }

            candidates.push(CandidateTx {
                serialized_len: transaction
                    .zcash_serialize_to_vec()
                    .expect("transaction from a block vector should serialize")
                    .len(),
                transaction,
                height: Height(height),
                time: block.header.time,
                counts,
            });
        }
    }

    (candidates, stats)
}

fn build_workload(case: &BenchmarkCase, candidates: &[CandidateTx]) -> Option<Workload> {
    let (target_action_counts, selected) = select_best_block_workload(case, candidates)?;

    let mut stats = WorkloadStats::default();
    let known_outpoint_hashes = Arc::new(HashSet::new());
    let known_utxos = Arc::new(HashMap::new());

    let requests = selected
        .iter()
        .map(|&index| {
            let candidate = &candidates[index];

            stats.serialized_bytes += candidate.serialized_len;
            stats.action_counts += candidate.counts;
            stats.verifier_checks += candidate.counts.verifier_check_counts();

            Request::Block {
                transaction_hash: candidate.transaction.hash(),
                transaction: candidate.transaction.clone(),
                known_outpoint_hashes: known_outpoint_hashes.clone(),
                known_utxos: known_utxos.clone(),
                height: candidate.height,
                time: candidate.time,
            }
        })
        .collect();

    stats.unique_transactions = selected.iter().copied().collect::<HashSet<_>>().len();
    stats.repeated_transactions = selected.len();
    stats.modeled_block_bytes = modeled_block_bytes(stats.serialized_bytes, selected.len());

    assert_eq!(
        stats.action_counts.shielded_pool_actions().counts,
        target_action_counts.counts,
        "selected workload must exactly match the requested shielded pool action mix",
    );
    assert!(
        stats.modeled_block_bytes <= max_block_bytes(),
        "selected workload must fit under the max block size",
    );

    Some(Workload {
        requests,
        target_action_counts,
        stats,
    })
}

fn select_best_block_workload(
    case: &BenchmarkCase,
    candidates: &[CandidateTx],
) -> Option<(ShieldedActionCounts, Vec<usize>)> {
    let mut best = None;

    for total_actions in 1..=max_total_actions(case.action_mix, candidates)? {
        let target_counts = case.action_mix.target_counts(total_actions);
        let Some(selected) = select_candidates_for_counts(target_counts, candidates) else {
            continue;
        };
        let tx_bytes = selected_tx_bytes(&selected, candidates);
        let block_bytes = modeled_block_bytes(tx_bytes, selected.len());

        if block_bytes <= max_block_bytes()
            && best
                .as_ref()
                .is_none_or(|(best_actions, best_bytes, _, _)| {
                    total_actions > *best_actions
                        || (total_actions == *best_actions && block_bytes > *best_bytes)
                })
        {
            best = Some((total_actions, block_bytes, target_counts, selected));
        }
    }

    best.map(|(_, _, target_counts, selected)| (target_counts, selected))
}

fn select_candidates_for_counts(
    target_counts: ShieldedActionCounts,
    candidates: &[CandidateTx],
) -> Option<Vec<usize>> {
    let mut selected = Vec::new();

    for pool in SHIELDED_POOLS {
        let target_actions = target_counts.action_count(pool);

        if target_actions == 0 {
            continue;
        }

        let mut matching_indices: Vec<_> = candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.has_only_pool_actions(pool))
            .map(|(index, _)| index)
            .collect();

        matching_indices.sort_by_key(|index| Reverse(candidates[*index].pool_score(pool)));
        selected.extend(select_pool_candidates(
            pool,
            target_actions,
            &matching_indices,
            candidates,
        )?);
    }

    Some(selected)
}

fn select_pool_candidates(
    pool: ShieldedPool,
    target_actions: usize,
    matching_indices: &[usize],
    candidates: &[CandidateTx],
) -> Option<Vec<usize>> {
    let mut previous_selection = vec![None; target_actions.saturating_add(1)];
    previous_selection[0] = Some((0, 0, usize::MAX));

    for selected_actions in 0..=target_actions {
        let Some((selected_bytes, _, _)) = previous_selection[selected_actions] else {
            continue;
        };

        for &index in matching_indices {
            let candidate = &candidates[index];
            let next_actions = selected_actions.saturating_add(candidate.counts.action_count(pool));
            let next_bytes = selected_bytes + candidate.serialized_len;

            if next_actions <= target_actions
                && previous_selection[next_actions]
                    .is_none_or(|(best_bytes, _, _)| next_bytes < best_bytes)
            {
                previous_selection[next_actions] = Some((next_bytes, selected_actions, index));
            }
        }
    }

    previous_selection[target_actions]?;

    let mut selected = Vec::new();
    let mut remaining_actions = target_actions;

    while remaining_actions > 0 {
        let (_, previous_actions, index) = previous_selection[remaining_actions]?;

        selected.push(index);
        remaining_actions = previous_actions;
    }

    Some(selected)
}

fn max_total_actions(mix: ShieldedActionMix, candidates: &[CandidateTx]) -> Option<usize> {
    SHIELDED_POOLS
        .iter()
        .copied()
        .filter(|pool| mix.percent(*pool) > 0)
        .map(|pool| {
            let percent = usize::from(mix.percent(pool));

            candidates
                .iter()
                .filter(|candidate| candidate.has_only_pool_actions(pool))
                .map(|candidate| {
                    let action_count = candidate.counts.action_count(pool);
                    let max_repeats = max_block_bytes() / candidate.serialized_len;

                    max_repeats * action_count * 100 / percent + SHIELDED_POOL_COUNT
                })
                .max()
        })
        .min()
        .flatten()
}

fn selected_tx_bytes(selected: &[usize], candidates: &[CandidateTx]) -> usize {
    selected
        .iter()
        .map(|&index| candidates[index].serialized_len)
        .sum()
}

fn modeled_block_bytes(tx_bytes: usize, tx_count: usize) -> usize {
    MAINNET_BLOCK_HEADER_BYTES + compact_size_len(tx_count) + tx_bytes
}

fn compact_size_len(count: usize) -> usize {
    match count {
        0..=252 => 1,
        253..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
}

fn print_workload_metadata(case: &BenchmarkCase, workload: &Workload) {
    let requested_actions = workload.target_action_counts;
    let actual_actions = workload.stats.action_counts.shielded_pool_actions();
    let actual_total_actions = actual_actions.total();
    let stats = &workload.stats;

    println!(
        "worst_case_tx_verification: case={} mode=tx verifier repeated workload target_block_bytes={} actual_block_bytes={} actual_tx_bytes={} block_fill_percent={:.2} block_bytes_remaining={} actual_shielded_pool_actions={} unique_txs={} repeated_txs={} rayon_threads={} tokio_worker_threads={} transparent_prevouts_allowed={}",
        case.name,
        max_block_bytes(),
        stats.modeled_block_bytes,
        stats.serialized_bytes,
        percent(stats.modeled_block_bytes, max_block_bytes()),
        max_block_bytes() - stats.modeled_block_bytes,
        actual_total_actions,
        stats.unique_transactions,
        stats.repeated_transactions,
        case.rayon_threads,
        case.tokio_worker_threads,
        ALLOW_TRANSPARENT_PREVOUTS_WITHOUT_UTXOS,
    );
    println!(
        "worst_case_tx_verification: case={} workload_source=mainnet_test_vectors workload_validity=repeated_txs_not_consensus_block selection_strategy={}",
        case.name,
        WORKLOAD_SELECTION_STRATEGY,
    );
    println!(
        "worst_case_tx_verification: case={} requested_pool_actions {} requested_pool_percent {}",
        case.name,
        pool_action_fields(requested_actions),
        pool_mix_fields(case.action_mix),
    );
    println!(
        "worst_case_tx_verification: case={} actual_pool_actions {}",
        case.name,
        pool_action_percent_fields(actual_actions, actual_total_actions),
    );
    println!(
        "worst_case_tx_verification: case={} raw_actions transparent_inputs={} sapling_spends={} sapling_outputs={} orchard_actions={} sprout_joinsplits={}",
        case.name,
        stats.action_counts.transparent_inputs,
        stats.action_counts.sapling_spends,
        stats.action_counts.sapling_outputs,
        stats.action_counts.orchard_actions,
        stats.action_counts.sprout_joinsplits,
    );
    println!(
        "worst_case_tx_verification: case={} verifier_checks sapling_bundles={} orchard_bundles={} sprout_joinsplit_proofs={} sprout_signatures={}",
        case.name,
        stats.verifier_checks.sapling_bundles,
        stats.verifier_checks.orchard_bundles,
        stats.verifier_checks.sprout_joinsplit_proofs,
        stats.verifier_checks.sprout_signatures,
    );
}

fn pool_action_fields(counts: ShieldedActionCounts) -> String {
    pool_fields(|pool| format!("{}={}", pool.name(), counts.action_count(pool)))
}

fn pool_mix_fields(mix: ShieldedActionMix) -> String {
    pool_fields(|pool| format!("{}={}", pool.name(), mix.percent(pool)))
}

fn pool_action_percent_fields(counts: ShieldedActionCounts, total: usize) -> String {
    pool_fields(|pool| {
        let actions = counts.action_count(pool);

        format!(
            "{}={} ({:.2}%)",
            pool.name(),
            actions,
            percent(actions, total)
        )
    })
}

fn pool_fields(field: impl Fn(ShieldedPool) -> String) -> String {
    SHIELDED_POOLS.map(field).join(" ")
}

fn percent(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        let count = u32::try_from(count).expect("benchmark action counts fit in u32");
        let total = u32::try_from(total).expect("benchmark action counts fit in u32");

        f64::from(count) * 100.0 / f64::from(total)
    }
}

fn max_block_bytes() -> usize {
    usize::try_from(MAX_BLOCK_BYTES).expect("Zcash max block bytes fit in usize")
}

fn make_transaction_verifier(buffer_bound: usize) -> TxVerifier {
    let verifier = tx::Verifier::new(&Network::Mainnet, BenchmarkState, closed_mempool_setup_rx());

    Buffer::new(BoxService::new(verifier), buffer_bound)
}

fn closed_mempool_setup_rx() -> oneshot::Receiver<BenchmarkMempool> {
    oneshot::channel().1
}

async fn verify_requests(verifier: TxVerifier, requests: &[Request]) -> usize {
    let mut futures = FuturesUnordered::new();

    for request in requests.iter().cloned() {
        let mut verifier = verifier.clone();

        futures.push(async move {
            verifier
                .ready()
                .await
                .expect("transaction verifier should always be ready")
                .call(request)
                .await
        });
    }

    let mut verified = 0;

    while let Some(result) = futures.next().await {
        result.expect("benchmark transaction should verify successfully");
        verified += 1;
    }

    assert_eq!(
        verified,
        requests.len(),
        "all benchmark transactions should be verified",
    );

    verified
}

impl CandidateTx {
    fn has_only_pool_actions(&self, pool: ShieldedPool) -> bool {
        self.counts.action_count(pool) > 0
            && SHIELDED_POOLS
                .iter()
                .copied()
                .filter(|candidate_pool| *candidate_pool != pool)
                .all(|candidate_pool| self.counts.action_count(candidate_pool) == 0)
    }

    fn pool_score(&self, pool: ShieldedPool) -> (usize, usize, usize, usize) {
        (
            self.counts.action_count(pool),
            self.serialized_len,
            self.counts.sapling_spends,
            self.counts.sapling_outputs,
        )
    }
}

impl ShieldedPool {
    const fn index(self) -> usize {
        match self {
            ShieldedPool::Sapling => 0,
            ShieldedPool::Orchard => 1,
            ShieldedPool::Sprout => 2,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            ShieldedPool::Sapling => "sapling",
            ShieldedPool::Orchard => "orchard",
            ShieldedPool::Sprout => "sprout",
        }
    }
}

impl ActionCounts {
    fn from_transaction(transaction: &Transaction) -> Self {
        Self {
            transparent_inputs: transaction
                .inputs()
                .iter()
                .filter(|input| matches!(input, transparent::Input::PrevOut { .. }))
                .count(),
            sapling_spends: transaction.sapling_spends_per_anchor().count(),
            sapling_outputs: transaction.sapling_outputs().count(),
            orchard_actions: transaction.orchard_actions().count(),
            sprout_joinsplits: transaction.joinsplit_count(),
        }
    }

    fn shielded_pool_actions(&self) -> ShieldedActionCounts {
        ShieldedActionCounts {
            counts: SHIELDED_POOLS.map(|pool| self.action_count(pool)),
        }
    }

    fn action_count(&self, pool: ShieldedPool) -> usize {
        match pool {
            ShieldedPool::Sapling => self.sapling_spends + self.sapling_outputs,
            ShieldedPool::Orchard => self.orchard_actions,
            ShieldedPool::Sprout => self.sprout_joinsplits,
        }
    }

    fn verifier_check_counts(&self) -> VerifierCheckCounts {
        VerifierCheckCounts {
            sapling_bundles: usize::from(self.sapling_spends + self.sapling_outputs > 0),
            orchard_bundles: usize::from(self.orchard_actions > 0),
            sprout_joinsplit_proofs: self.sprout_joinsplits,
            sprout_signatures: usize::from(self.sprout_joinsplits > 0),
        }
    }
}

impl ShieldedActionMix {
    const fn new(sapling_percent: u8, orchard_percent: u8, sprout_percent: u8) -> Self {
        Self {
            percents: [sapling_percent, orchard_percent, sprout_percent],
        }
    }

    fn total_percent(&self) -> u16 {
        self.percents
            .iter()
            .map(|percent| u16::from(*percent))
            .sum()
    }

    fn percent(&self, pool: ShieldedPool) -> u8 {
        self.percents[pool.index()]
    }

    fn target_counts(&self, total_actions: usize) -> ShieldedActionCounts {
        let mut targets = self
            .percents
            .map(|percent| total_actions * usize::from(percent) / 100);
        let mut remainders = self
            .percents
            .map(|percent| total_actions * usize::from(percent) % 100);
        let assigned_actions: usize = targets.iter().sum();

        for _ in assigned_actions..total_actions {
            let (index, _) = remainders
                .iter()
                .enumerate()
                .max_by_key(|(index, remainder)| (**remainder, self.percents[*index]))
                .expect("shielded pool action percentages should not be empty");

            targets[index] += 1;
            remainders[index] = 0;
        }

        ShieldedActionCounts { counts: targets }
    }
}

impl ShieldedActionCounts {
    fn total(&self) -> usize {
        self.counts.iter().sum()
    }

    fn action_count(&self, pool: ShieldedPool) -> usize {
        self.counts[pool.index()]
    }
}

impl std::ops::AddAssign for ActionCounts {
    fn add_assign(&mut self, rhs: Self) {
        self.transparent_inputs += rhs.transparent_inputs;
        self.sapling_spends += rhs.sapling_spends;
        self.sapling_outputs += rhs.sapling_outputs;
        self.orchard_actions += rhs.orchard_actions;
        self.sprout_joinsplits += rhs.sprout_joinsplits;
    }
}

impl std::ops::AddAssign for VerifierCheckCounts {
    fn add_assign(&mut self, rhs: Self) {
        self.sapling_bundles += rhs.sapling_bundles;
        self.orchard_bundles += rhs.orchard_bundles;
        self.sprout_joinsplit_proofs += rhs.sprout_joinsplit_proofs;
        self.sprout_signatures += rhs.sprout_signatures;
    }
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .noise_threshold(0.05)
        .sample_size(10)
        .measurement_time(Duration::from_secs(30));
    targets = worst_case_tx_verification
);
criterion_main!(benches);
