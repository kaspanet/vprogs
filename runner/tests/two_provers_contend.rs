//! End-to-end two-prover contention test (dev mode, CPU).
//!
//! Two independent prover nodes settle the SAME dev covenant against ONE simnet L1, racing from
//! different fee addresses with different bundle-size ranges, advancing each other. This proves the
//! settler's competitor-reconciliation resilience: under contention neither worker panics, both
//! provers land settlements, and the covenant continuation chain stays a single contiguous
//! spend-chain (the two provers cooperatively advance ONE covenant rather than forking it).
//!
//! Runs only under `RISC0_DEV_MODE=1` (dev stub proofs + dev redeem; no GPU). The production /
//! CUDA path is covered by `zk/backend/risc0/test-suite/tests/settlement_l1_e2e.rs`.

use std::{collections::HashMap, ops::RangeInclusive, sync::Arc, time::Duration};

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{
    config::params::{ForkActivation, Params},
    constants::{SOMPI_PER_KASPA, TX_VERSION_TOCCATA},
    mass::BlockMassLimits,
    network::{NetworkId, NetworkType},
    subnets::SubnetworkId,
    tx::{Transaction, TransactionOutpoint},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::pay_to_address_script;
use kaspa_wrpc_client::prelude::*;
use secp256k1::Keypair;
use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard, watch};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_types::{L1TransactionCovenantExt, SettlementInfo};
use vprogs_l1_wallet::encode_activity_payload;
use vprogs_node_test_utils::L1Node;
use vprogs_runner::{
    BridgeObservers, BridgeParams, Elfs, ProvingParams, RunnerNode, RunnerStore, SettlementQueue,
    build_proving_node,
};
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_settler::{
    AlternationPacer, CovenantState, SettlementMode, SettlementWorkerConfig, dev_bootstrap_redeem,
    run as run_settlement_worker,
};
use vprogs_zk_backend_risc0_test_suite::{
    TEST_SUBNETWORK_ID, batch_aggregator_elf, batch_processor_elf, dev_mode_enabled, test_lane_key,
    transaction_processor_elf,
};

/// Serializes the two settlement tests in this binary. Each spins up a full in-process L1 node and
/// two dev-proving provers; running both at once oversubscribes the CPU, so each range's proving
/// misses its acceptance window and the contention assertions flake. The tests share nothing else,
/// so a single lock held for each test's duration is enough to keep them from racing for the host.
static SETTLEMENT_TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// Acquires the cross-test serialization lock for the duration of a settlement test.
async fn serialize_settlement_test() -> MutexGuard<'static, ()> {
    SETTLEMENT_TEST_LOCK.lock().await
}

/// Value locked in the covenant UTXO at bootstrap (1 KAS), matching the e2e settlement tests.
const COVENANT_VALUE: u64 = SOMPI_PER_KASPA;

/// Lane subnetwork both provers route their L2 carrier txs onto: the shared [`TEST_SUBNETWORK_ID`]
/// the guest derives its lane_key from, so the chain's lane-key bucket and the guest's committed
/// lane_key match (see the e2e test's `L2_LANE_SUBNET` doc for the divergence this avoids).
const LANE_SUBNET: SubnetworkId = TEST_SUBNETWORK_ID;

/// Bridge lane-finality window. Consensus only falls back to the parent block's `seq_commit`
/// (rather than the lane tip) as the lane parent-ref when the lane has no live SMT entry: on its
/// FIRST activation, or after it has gone silent past the real finality window (~432k blocks, never
/// reached in a test). The bridge models "no live entry" as `blue_score - parent.lane_blue_score >
/// finality_depth`. With the real simnet finality_depth the first activation never trips that
/// bound, so the bridge would pick the zero parent lane tip while consensus picks the parent
/// seq_commit, and the guest's committed `new_seq_commit` diverges from the chain's
/// `accepted_id_merkle_root`. A small window fixes this: the first carrier's blue score is well
/// above it (parent `lane_blue_score` is 0, so first activation expires → seq_commit, matching
/// consensus), while consecutive carriers stay within it (gap ~2 blue score → lane alive → lane
/// tip, matching consensus, which never expires a re-touched lane in-test).
const LANE_FINALITY_DEPTH: u64 = 10;

/// Sompi each funding output carries, and how many each prover address is seeded with. A prover's
/// settler funds every settlement fee from its own address, so it needs several spendable UTXOs to
/// fund a chain of settlements under contention.
const FUND_VALUE: u64 = 100_000_000;
const FUND_COUNT: usize = 6;

/// Per-prover wiring kept alive for the duration of the run. Dropping the [`RunnerNode`] tears the
/// prover's bridge and pipeline down, so the test holds both for the whole driver loop.
struct Prover {
    /// The proving node (bridge + pipeline). Explicitly shut down at teardown so its worker, and
    /// the RocksDB store it holds, is released before `_db_dir` is reclaimed.
    node: RunnerNode,
    /// The settler task. Awaited at the end; `Ok(())` proves it did not panic on a competitor.
    settler: tokio::task::JoinHandle<()>,
    /// Latch the test opens to tear the settler down gracefully.
    shutdown: AtomicAsyncLatch,
    /// Scratch dir backing the prover's RocksDB store. Held for the run, then reclaimed on drop
    /// after the node is shut down so the store has already closed its files.
    _db_dir: TempDir,
}

#[tokio::test(flavor = "multi_thread")]
async fn two_provers_contend() {
    // Dev-only: real proofs need a GPU. Under non-dev builds this test has nothing to assert.
    if !dev_mode_enabled() {
        eprintln!(
            "skipping two_provers_contend: RISC0_DEV_MODE!=1 - the contention scenario runs dev \
             stub proofs + the dev redeem on CPU; the production path is covered by \
             settlement_l1_e2e under CUDA",
        );
        return;
    }
    // Serialize against the other settlement test: two at once oversubscribe the CPU and flake.
    let _serial = serialize_settlement_test().await;

    // === Step 0: simnet L1 ===
    // Mirror the e2e settlement config: instant coinbase maturity, covenants always active, a
    // raised block mass cap (the dev settlements are small but the cap keeps us clear of the
    // default 500k).
    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.toccata_activation = ForkActivation::always();
            p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    // Coinbase feeds the bootstrap, both funding txs, and ongoing carrier fees; mine generously.
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the shared dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key);
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
    eprintln!("dev covenant bootstrapped: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The settler's `run` confirms this outpoint and fills daa_score itself; we hand each prover
    // its own clone of the initial state.
    let bootstrap_outpoint = TransactionOutpoint::new(boot_txid, 0);
    let initial_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
        spk: bootstrap_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };

    // === Step 2: two distinct prover keypairs, each funded at its own address ===
    let kp_a = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let kp_b = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a = prover_address(&kp_a, network_id);
    let addr_b = prover_address(&kp_b, network_id);
    assert_ne!(addr_a, addr_b, "the two provers must settle from distinct addresses");
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;
    l1.fund_address(&addr_b, FUND_VALUE, FUND_COUNT).await;
    eprintln!("funded prover A address {addr_a}");
    eprintln!("funded prover B address {addr_b}");

    // === Step 3: spin up both provers ===
    // Both bundle over the same size range, so neither structurally forms a bundle first. The
    // settlers share an `AlternationPacer`: whoever lands a settlement waits for the other to land
    // the next, so they strictly alternate. The deferring prover finds its covenant outpoint
    // already spent (its bundle superseded), skips it, and reconciles to the other's advance before
    // settling the following range. This keeps the spend-chain a single contiguous chain that both
    // provers advance, and makes the both-provers-settled outcome deterministic instead of a
    // per-range coin flip that can starve one prover.
    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);
    let pacer = Arc::new(AlternationPacer::new());

    // Both fresh-deploy provers seed from the deploy block (`Some(block_deploy)`), exactly as the
    // binary's fresh-deploy path does (`main.rs` persists the captured sink as the seed and threads
    // it into both the bridge and the settler config). The `start_from = Some` settler starts from
    // its live settlement handle: at startup the handle is empty (no settlement yet), so it leaves
    // `cov` at the unspent bootstrap and the loop's mid-stream adoption advances it as the bridge
    // publishes each settlement off the replayed chain - no L1 chain scan.
    let prover_a = spawn_prover(
        &l1,
        "A",
        kp_a,
        addr_a.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        initial_covenant.clone(),
        elfs,
        Some((0, pacer.clone())),
        Some(block_deploy),
    )
    .await;
    let prover_b = spawn_prover(
        &l1,
        "B",
        kp_b,
        addr_b.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        initial_covenant.clone(),
        elfs,
        Some((1, pacer.clone())),
        Some(block_deploy),
    )
    .await;

    // === Step 4: drive lane activity ===
    // Each iteration opens ONE settlement range: it mines a full bundle's worth of lane carriers
    // (`CARRIERS_PER_RANGE`, the bundle minimum), then several acceptance blocks, then sleeps long
    // enough for BOTH provers to prove, race, settle, and confirm that range before the next opens.
    // Pacing one range at a time (rather than streaming carriers) keeps neither prover perpetually
    // a confirmation-latency behind the other: each range is a fresh, jittered spend race, so
    // over many ranges both provers win some. Settlements the settlers submit to the mempool
    // are pulled into the next mined block.
    const DRIVER_ITERS: usize = 10;
    const CARRIERS_PER_RANGE: usize = 2;
    for i in 0..DRIVER_ITERS {
        eprintln!("driver: top of iteration {i}");
        for _ in 0..CARRIERS_PER_RANGE {
            let payload = encode_activity_payload(
                &[AccessMetadata::write(ResourceId::for_test(1))],
                &[1, 2, 3],
            );
            let carrier = match tokio::time::timeout(
                Duration::from_secs(10),
                l1.build_subnet_payload_transactions(
                    vec![payload],
                    LANE_SUBNET,
                    TX_VERSION_TOCCATA,
                ),
            )
            .await
            {
                Ok(txs) => txs.into_iter().next().expect("carrier tx"),
                Err(_) => {
                    panic!("driver: build_subnet_payload_transactions timed out at iteration {i}")
                }
            };
            l1.mine_block(std::slice::from_ref(&carrier)).await;
        }
        // Mine acceptance blocks. Each acceptance block pulls pending settlements out of the
        // mempool and onto the chain, where both bridges observe them as the covenant's
        // `last_settlement`. The trailing prover then ADOPTS the advanced covenant instead
        // of orphaning a stale settlement against it.
        eprintln!("driver: iteration {i} mined carriers, mining acceptance");
        for _ in 0..5 {
            l1.mine_blocks(1).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        eprintln!("driver: iteration {i} mined acceptance");
        if i % 5 == 0 {
            eprintln!("driver: iteration {i}/{DRIVER_ITERS}");
        }
    }

    // === Step 5: drain in-flight settlements, then tear the settlers down ===
    // Keep mining acceptance blocks so settlements still in the mempool land and confirm, using the
    // SAME one-block-at-a-time pacing as the driver loop's acceptance phase: mine one block, then
    // pause long enough for the trailing prover to adopt the advance before the next block lands.
    // Mining several blocks back-to-back here would let whichever prover is ahead settle the whole
    // backlog before the other adopts, starving it and making the both-provers-settled assertion
    // flaky. Poll the covenant chain and stop once it stops growing for a few rounds (or a bounded
    // ceiling), so the tail settlements are captured with the race still alternating.
    let mut prev_len = 0usize;
    let mut stable_rounds = 0;
    for round in 0..40 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if len == prev_len {
            stable_rounds += 1;
        } else {
            stable_rounds = 0;
        }
        prev_len = len;
        if stable_rounds >= 3 && len >= 3 {
            break;
        }
        if round % 5 == 0 {
            eprintln!("drain: round {round}, covenant chain length {len}");
        }
    }

    prover_a.shutdown.open();
    prover_b.shutdown.open();
    let join_a = prover_a.settler.await;
    let join_b = prover_b.settler.await;

    // Tear the proving nodes down before their scratch dirs are reclaimed: `shutdown` joins each
    // node's worker, releasing the RocksDB store, so the held `TempDir` is removed only after the
    // store has closed its files. Done before the assertions below so a failing assertion still
    // unwinds cleanly rather than racing the directory removal at drop.
    prover_a.node.shutdown();
    prover_b.node.shutdown();

    // === Assertion 1: no panic (the resilience claim) ===
    // A settler that hit the old panic-on-competitor-advance returns Err (the task panicked).
    // Surface the join result before any other assertion so a panic shows up here, not as a
    // timeout.
    assert!(join_a.is_ok(), "prover A settler panicked: {join_a:?}");
    assert!(join_b.is_ok(), "prover B settler panicked: {join_b:?}");

    // === Assertion 2 + 3: contiguous chain + both provers settled ===
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let change_spk_a = pay_to_address_script(&addr_a);
    let change_spk_b = pay_to_address_script(&addr_b);

    let mut per_prover: HashMap<&'static str, usize> = HashMap::new();
    let mut expected_input = bootstrap_outpoint;
    for (pos, link) in chain.iter().enumerate() {
        // Attribute by the settlement's change output: each prover funds its fee from its own key,
        // so its change output pays back to its own address.
        let attributed = if link.change_spks.contains(&change_spk_a) {
            "A"
        } else if link.change_spks.contains(&change_spk_b) {
            "B"
        } else {
            "?"
        };
        *per_prover.entry(attributed).or_default() += 1;
        eprintln!(
            "settlement #{pos}: tx={} prover={attributed} input={} output=({}:0)",
            link.tx_id, link.covenant_input, link.tx_id,
        );

        // === Assertion 3: contiguity ===
        // Each settlement's covenant input must be the previous settlement's covenant output
        // (output 0), starting from the bootstrap outpoint. A single linear spend-chain proves the
        // two provers advanced ONE covenant cooperatively with no divergent fork / double-settle.
        assert_eq!(
            link.covenant_input, expected_input,
            "settlement #{pos} ({}) must spend the previous covenant output {expected_input}; \
             the continuation chain forked",
            link.tx_id,
        );
        expected_input = TransactionOutpoint::new(link.tx_id, 0);
    }

    let count_a = *per_prover.get("A").unwrap_or(&0);
    let count_b = *per_prover.get("B").unwrap_or(&0);
    eprintln!(
        "settlements: total={} A={count_a} (addr {addr_a}) B={count_b} (addr {addr_b})",
        chain.len(),
    );

    // The chain must have actually advanced under contention.
    assert!(
        chain.len() >= 3,
        "expected a covenant chain of at least 3 settlements under contention, got {}",
        chain.len(),
    );
    // Both provers must have landed at least one settlement (proves the race, not a single winner).
    assert!(count_a >= 1, "prover A (addr {addr_a}) produced no settlements");
    assert!(count_b >= 1, "prover B (addr {addr_b}) produced no settlements");

    // No settlement landed on a DAG side-branch: the selected-chain count equals the DAG-wide
    // count.
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    l1.shutdown().await;
}

/// Two provers contend over one covenant with DIVERGENT bundle sizes so their boundaries never
/// align: prover A bundles `5..=5`, prover B bundles `3..=3`, so each of B's short settlements
/// covers only part of A's longer in-flight bundle, leaving A a surviving suffix it proved but
/// could not settle (B's shorter range superseded the whole bundle). The driver runs several
/// contended ranges, then stops injecting fresh ranges and runs a bounded acceptance-only drain,
/// exercising the retain + re-aggregation path that keeps the provers converging once fresh
/// activity stops.
///
/// This is integration coverage of the contention path: it asserts the two provers converge on a
/// single non-forked continuation chain that advances to settle their proved work, and that neither
/// settler panics while adopting competitors and re-forming superseded suffixes. It is NOT a
/// decisive isolation of the re-form path on its own: the contention livelock is rare and
/// nondeterministic, and the covenant chain self-heals through whichever prover settles each range,
/// so a whole-system advance cannot be attributed solely to re-aggregation. The decisive,
/// deterministic guard for the re-form drain decision (an unmatched settlement boundary must drop
/// nothing, never clear the retained window) lives in the `settled_prefix` unit tests in
/// `vprogs-zk-aggregate-prover`.
#[tokio::test(flavor = "multi_thread")]
async fn two_provers_reform_superseded_suffix() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping two_provers_reform_superseded_suffix: RISC0_DEV_MODE!=1 - the re-form \
             scenario runs dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    // === Step 0: simnet L1 (same config as two_provers_contend) ===
    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.toccata_activation = ForkActivation::always();
            p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the shared dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key);
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
    eprintln!("dev covenant bootstrapped: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let bootstrap_outpoint = TransactionOutpoint::new(boot_txid, 0);
    let initial_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
        spk: bootstrap_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };

    // === Step 2: two distinct, funded prover keypairs ===
    let kp_a = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let kp_b = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a = prover_address(&kp_a, network_id);
    let addr_b = prover_address(&kp_b, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;
    l1.fund_address(&addr_b, FUND_VALUE, FUND_COUNT).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);
    let pacer = Arc::new(AlternationPacer::new());

    // === Step 3: spin up both provers with DIVERGENT bundle sizes ===
    // A bundles fives, B bundles threes: their bundle boundaries never align, so each settlement
    // covers only part of the other's in-flight bundle and leaves a surviving suffix that the loser
    // must re-aggregate against the adopted tip rather than drop.
    let prover_a = spawn_prover(
        &l1,
        "A",
        kp_a,
        addr_a.clone(),
        5..=5,
        network_id,
        &params,
        lane_key,
        covenant_id,
        initial_covenant.clone(),
        elfs,
        Some((0, pacer.clone())),
        Some(block_deploy),
    )
    .await;
    let prover_b = spawn_prover(
        &l1,
        "B",
        kp_b,
        addr_b.clone(),
        3..=3,
        network_id,
        &params,
        lane_key,
        covenant_id,
        initial_covenant,
        elfs,
        Some((1, pacer.clone())),
        Some(block_deploy),
    )
    .await;

    // === Step 4: drive several contended ranges (carriers + acceptance) ===
    // Each range mines a burst of consecutive lane carriers so both bundle sizes can form (A needs
    // 5, B needs 3 consecutively ready), then acceptance blocks so settlements land and both
    // bridges publish the covenant's advancing `last_settlement`. The divergent boundaries
    // guarantee at least one prover's bundle is superseded each range, exercising retain +
    // re-aggregation.
    const DRIVER_ITERS: usize = 8;
    const CARRIERS_PER_RANGE: usize = 5;
    for i in 0..DRIVER_ITERS {
        for _ in 0..CARRIERS_PER_RANGE {
            let payload = encode_activity_payload(
                &[AccessMetadata::write(ResourceId::for_test(1))],
                &[1, 2, 3],
            );
            let carrier = l1
                .build_subnet_payload_transactions(vec![payload], LANE_SUBNET, TX_VERSION_TOCCATA)
                .await
                .into_iter()
                .next()
                .expect("carrier tx");
            l1.mine_block(std::slice::from_ref(&carrier)).await;
        }
        for _ in 0..5 {
            l1.mine_blocks(1).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if i % 4 == 0 {
            let len =
                covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
            eprintln!("reform driver: iteration {i}/{DRIVER_ITERS}, covenant chain length {len}");
        }
    }

    // Pre-drain chain length: residual suffixes proved past here may still be in flight, superseded
    // and awaiting re-aggregation against the adopted tip.
    let pre_drain_len =
        covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert!(
        pre_drain_len >= 3,
        "the two provers must land >=3 settlements under contention before the drain, got {pre_drain_len}",
    );

    // === Step 5: acceptance-only drain - NO fresh ranges ===
    // Mine acceptance blocks only (the divergent ranges are spent), one at a time with a pause so a
    // re-formed suffix has time to prove and confirm. The retain + re-aggregation path keeps the
    // contending provers converging rather than wedging once fresh activity stops; stop once the
    // chain has advanced and stabilized, or at a bounded ceiling.
    let mut prev_len = pre_drain_len;
    let mut stable_rounds = 0;
    for round in 0..40 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if len == prev_len {
            stable_rounds += 1;
        } else {
            stable_rounds = 0;
        }
        prev_len = len;
        if stable_rounds >= 3 && len > pre_drain_len {
            break;
        }
        if round % 5 == 0 {
            eprintln!("reform drain: round {round}, covenant chain length {len}");
        }
    }

    prover_a.shutdown.open();
    prover_b.shutdown.open();
    let join_a = prover_a.settler.await;
    let join_b = prover_b.settler.await;
    prover_a.node.shutdown();
    prover_b.node.shutdown();

    // Neither settler panicked while adopting competitors and re-forming superseded suffixes.
    assert!(join_a.is_ok(), "prover A settler panicked: {join_a:?}");
    assert!(join_b.is_ok(), "prover B settler panicked: {join_b:?}");

    // === Assertions: the contending provers converged on one contiguous, advancing chain ===
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let mut expected_input = bootstrap_outpoint;
    for (pos, link) in chain.iter().enumerate() {
        assert_eq!(
            link.covenant_input, expected_input,
            "settlement #{pos} ({}) must spend the previous covenant output {expected_input}; the \
             continuation chain forked",
            link.tx_id,
        );
        expected_input = TransactionOutpoint::new(link.tx_id, 0);
    }

    eprintln!(
        "reform: final covenant chain length = {} (pre_drain_len = {pre_drain_len})",
        chain.len(),
    );

    // The chain kept advancing through the acceptance-only drain rather than wedging: the
    // contending provers adopted each other's settlements and re-formed superseded suffixes
    // instead of dropping them. (This advance is convergence evidence, not a re-form isolation
    // - see the doc comment and the `settled_prefix` unit tests for the decisive drain-decision
    // guard.)
    assert!(
        chain.len() > pre_drain_len,
        "the acceptance-only drain must keep the chain advancing past the pre-drain length \
         ({} <= {pre_drain_len}); the contending provers wedged",
        chain.len(),
    );

    // No settlement landed on a DAG side-branch: every settlement chained off the adopted tip.
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    l1.shutdown().await;
}

/// A second prover joins an already-advancing covenant through the catch-up path: an EMPTY store,
/// a `CovenantState` reconstructed from only the covenant id + bootstrap txid (no bootstrap), and a
/// bridge seeded at the deploy block via `start_from` so it replays L1 forward from there. This is
/// how a node joins a running covenant it did not create.
///
/// Prover A bootstraps the covenant and lands the first settlements alone; prover B then joins via
/// catch-up, reconstructs the never-advanced covenant from env-style inputs (the same state
/// `bootstrap_dev_covenant` produces), and self-heals to A's on-chain advance. We assert B catches
/// up and lands at least one settlement on the SAME single contiguous spend-chain.
#[tokio::test(flavor = "multi_thread")]
async fn prover_catches_up_to_existing_covenant() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping prover_catches_up_to_existing_covenant: RISC0_DEV_MODE!=1 - catch-up runs \
             dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    // Serialize against the other settlement test: two at once oversubscribe the CPU and flake.
    let _serial = serialize_settlement_test().await;

    // === Step 0: simnet L1 (same config as two_provers_contend) ===
    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.toccata_activation = ForkActivation::always();
            p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key);
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
    eprintln!("dev covenant bootstrapped: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let bootstrap_outpoint = TransactionOutpoint::new(boot_txid, 0);
    let initial_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
        spk: bootstrap_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };

    // === Step 2: fund both provers ===
    let kp_a = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let kp_b = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a = prover_address(&kp_a, network_id);
    let addr_b = prover_address(&kp_b, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;
    l1.fund_address(&addr_b, FUND_VALUE, FUND_COUNT).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);
    let pacer = Arc::new(AlternationPacer::new());

    // === Step 3: prover A holds the bootstrap state; prover B joins via the catch-up path ===
    // A is the original prover, seeded at the sink (seed_depth 0) with the bootstrap covenant
    // state, exactly as in two_provers_contend.
    //
    // B never bootstrapped: it reconstructs the covenant from ONLY the covenant id + bootstrap txid
    // (no on-chain bootstrap call), exactly as main.rs's settlement-mode catch-up branch does (same
    // `dev_bootstrap_redeem`, so the reconstructed P2SH SPK matches the on-chain UTXO), starts from
    // an EMPTY store, and seeds its bridge at the deploy block via `start_from` so it replays L1
    // forward from there to rebuild L2 state. This is how a 2nd prover joins the 1st prover's
    // covenant to contend.
    //
    // The two are spawned together while the bootstrap UTXO is still unspent: each settler confirms
    // that UTXO at startup, then they race and alternate, with the deferring prover adopting the
    // other's on-chain advance via the last_settlement self-heal. (A catch-up that joins a covenant
    // already settled past its bootstrap - where the bootstrap UTXO is spent before the joining
    // settler starts - is covered by `prover_catches_up_to_already_settled_covenant`, which
    // exercises the startup adopt-the-tip path.)
    let prover_a = spawn_prover(
        &l1,
        "A",
        kp_a,
        addr_a.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        initial_covenant,
        elfs,
        Some((0, pacer.clone())),
        None,
    )
    .await;

    let (_redeem, catchup_spk) = dev_bootstrap_redeem(&lane_key);
    let catchup_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: TransactionOutpoint::new(boot_txid, 0),
        spk: catchup_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };
    let prover_b = spawn_prover(
        &l1,
        "B",
        kp_b,
        addr_b.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        catchup_covenant,
        elfs,
        Some((1, pacer.clone())),
        Some(block_deploy),
    )
    .await;

    // === Step 4: drive both, then drain ===
    const DRIVER_ITERS: usize = 10;
    for i in 0..DRIVER_ITERS {
        eprintln!("catchup driver: iteration {i}");
        drive_range(&l1).await;
    }

    // The acceptance-only drain that catches a steady-state contention livelock (where a
    // proved-but-unsettled bundle a competitor supersedes must re-form against the adopted tip
    // instead of being dropped) lives in `two_provers_reform_superseded_suffix`, which drives
    // divergent bundle sizes and then drains with NO fresh ranges. This test keeps a
    // range-injecting drain because its focus is the catch-up join, not the re-form path.
    //
    // Range-injecting drain: keep offering fresh carrier ranges (which keeps the contention path
    // making progress) until the chain reaches the target and stabilizes.
    let mut prev_len = 0usize;
    let mut stable_rounds = 0;
    for _ in 0..40 {
        drive_range(&l1).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if len == prev_len {
            stable_rounds += 1;
        } else {
            stable_rounds = 0;
        }
        prev_len = len;
        if stable_rounds >= 3 && len >= 3 {
            break;
        }
    }

    prover_a.shutdown.open();
    prover_b.shutdown.open();
    let join_a = prover_a.settler.await;
    let join_b = prover_b.settler.await;
    prover_a.node.shutdown();
    prover_b.node.shutdown();

    assert!(join_a.is_ok(), "prover A settler panicked: {join_a:?}");
    assert!(join_b.is_ok(), "catch-up prover B settler panicked: {join_b:?}");

    // === Assertions: contiguous chain + the catch-up prover landed a settlement ===
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let change_spk_a = pay_to_address_script(&addr_a);
    let change_spk_b = pay_to_address_script(&addr_b);

    let mut count_b = 0usize;
    let mut expected_input = bootstrap_outpoint;
    for (pos, link) in chain.iter().enumerate() {
        if link.change_spks.contains(&change_spk_b) {
            count_b += 1;
        }
        let attributed = if link.change_spks.contains(&change_spk_a) {
            "A"
        } else if link.change_spks.contains(&change_spk_b) {
            "B"
        } else {
            "?"
        };
        eprintln!("catchup settlement #{pos}: tx={} prover={attributed}", link.tx_id);
        assert_eq!(
            link.covenant_input, expected_input,
            "settlement #{pos} ({}) must spend the previous covenant output {expected_input}; the \
             continuation chain forked",
            link.tx_id,
        );
        expected_input = TransactionOutpoint::new(link.tx_id, 0);
    }

    // The chain must have advanced under contention.
    assert!(
        chain.len() >= 3,
        "expected a covenant chain of at least 3 settlements under contention, got {}",
        chain.len(),
    );
    // The catch-up prover (empty store, no bootstrap, reconstructed covenant, replayed from the
    // deploy block) must have rebuilt state and landed at least one settlement on the shared chain
    // - the whole point of the catch-up path.
    assert!(
        count_b >= 1,
        "catch-up prover B (addr {addr_b}) landed no settlements; it failed to join the covenant",
    );

    // No settlement landed on a DAG side-branch (the fork the racy mid-loop adoption produced).
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    l1.shutdown().await;
}

/// A prover joins a covenant that has ALREADY settled past its bootstrap, so the bootstrap UTXO is
/// spent before the joining settler ever starts. This is the regression the resume/catch-up fix
/// closes: the settler's old startup hard-confirmed the bootstrap UTXO as unspent and PANICKED on
/// timeout, so a node joining an advanced covenant could never start. The fix times the bootstrap
/// confirm out instead and adopts the on-chain tip from the first bundle's `last_settlement`.
///
/// Prover A bootstraps and lands several settlements ALONE (no alternation partner), spending the
/// bootstrap UTXO. THEN prover B joins via the catch-up path with an EMPTY store, a `CovenantState`
/// reconstructed from only the covenant id (a PLACEHOLDER outpoint, exercising main.rs's no-txid
/// catch-up branch), and a bridge seeded at the deploy block. B must NOT panic at startup; it must
/// adopt A's advanced tip and land a settlement on the SAME single contiguous spend-chain.
#[tokio::test(flavor = "multi_thread")]
async fn prover_catches_up_to_already_settled_covenant() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping prover_catches_up_to_already_settled_covenant: RISC0_DEV_MODE!=1 - catch-up \
             runs dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    // === Step 0: simnet L1 (same config as the other settlement tests) ===
    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.toccata_activation = ForkActivation::always();
            p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key);
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
    eprintln!("dev covenant bootstrapped: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let bootstrap_outpoint = TransactionOutpoint::new(boot_txid, 0);
    let initial_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
        spk: bootstrap_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };

    // === Step 2: fund both provers ===
    let kp_a = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let kp_b = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a = prover_address(&kp_a, network_id);
    let addr_b = prover_address(&kp_b, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;
    l1.fund_address(&addr_b, FUND_VALUE, FUND_COUNT).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);

    // === Step 3: prover A settles ALONE until the bootstrap is well spent ===
    // No alternation partner, so A settles every range it forms; we drive until it has landed at
    // least 2 settlements (the bootstrap outpoint is spent by the first).
    let prover_a = spawn_prover(
        &l1,
        "A",
        kp_a,
        addr_a.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        initial_covenant,
        elfs,
        None,
        None,
    )
    .await;

    for i in 0..10 {
        drive_range(&l1).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("solo-A driver: iteration {i}, covenant chain length {len}");
        if len >= 2 {
            break;
        }
    }
    let chain_before_b =
        covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert!(
        chain_before_b >= 2,
        "prover A must land >=2 settlements (spending the bootstrap) before B joins, got {chain_before_b}",
    );

    // Shut A down before B joins, so B catches up and settles alone (no contention). The point of
    // this test is the startup adopt-the-tip path, not a fairness race: an established A would
    // out-compete a freshly-joined B for every spend, leaving B with nothing to attribute. B
    // joining a covenant whose bootstrap is ALREADY spent is the regression; that it then
    // advances the chain alone proves the adoption.
    let join_a = {
        prover_a.shutdown.open();
        let join = prover_a.settler.await;
        prover_a.node.shutdown();
        join
    };
    assert!(join_a.is_ok(), "prover A settler panicked: {join_a:?}");

    // === Step 4: prover B joins the already-settled covenant via the no-txid catch-up path ===
    // EMPTY store, a covenant reconstructed from ONLY the covenant id with a PLACEHOLDER outpoint
    // (covenant_id:0, exactly what main.rs's catch-up branch builds when no bootstrap txid is
    // supplied), and a bridge seeded at the deploy block. The bootstrap UTXO is already spent, so
    // B's settler must time the confirm out and adopt the on-chain tip rather than panic.
    let (_redeem, catchup_spk) = dev_bootstrap_redeem(&lane_key);
    let catchup_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: TransactionOutpoint::new(covenant_id, 0),
        spk: catchup_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };
    let prover_b = spawn_prover(
        &l1,
        "B",
        kp_b,
        addr_b.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        catchup_covenant,
        elfs,
        None,
        Some(block_deploy),
    )
    .await;

    // === Step 5: keep driving so B catches up and lands a settlement, then drain ===
    const DRIVER_ITERS: usize = 10;
    for i in 0..DRIVER_ITERS {
        eprintln!("already-settled catchup driver: iteration {i}");
        drive_range(&l1).await;
    }

    let mut prev_len = 0usize;
    let mut stable_rounds = 0;
    for _ in 0..40 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if len == prev_len {
            stable_rounds += 1;
        } else {
            stable_rounds = 0;
        }
        prev_len = len;
        if stable_rounds >= 3 && len > chain_before_b {
            break;
        }
    }

    prover_b.shutdown.open();
    let join_b = prover_b.settler.await;
    prover_b.node.shutdown();

    // === Assertion 1: B did NOT panic at startup (the regression this fix closes) ===
    assert!(
        join_b.is_ok(),
        "catch-up prover B settler panicked joining an already-settled covenant: {join_b:?}",
    );

    // === Assertion 2 + 3: single contiguous chain + B landed a settlement after joining ===
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let change_spk_b = pay_to_address_script(&addr_b);

    let mut count_b = 0usize;
    let mut expected_input = bootstrap_outpoint;
    for (pos, link) in chain.iter().enumerate() {
        if link.change_spks.contains(&change_spk_b) {
            count_b += 1;
        }
        assert_eq!(
            link.covenant_input, expected_input,
            "settlement #{pos} ({}) must spend the previous covenant output {expected_input}; the \
             continuation chain forked",
            link.tx_id,
        );
        expected_input = TransactionOutpoint::new(link.tx_id, 0);
    }

    assert!(
        chain.len() > chain_before_b,
        "the covenant chain must advance past A's solo settlements after B joins ({} <= {chain_before_b})",
        chain.len(),
    );
    // B, joining a covenant whose bootstrap was already spent, must have adopted the on-chain tip
    // and landed at least one settlement - the whole point of the fix.
    assert!(
        count_b >= 1,
        "catch-up prover B (addr {addr_b}) landed no settlements joining an already-settled covenant",
    );

    // The decisive anti-fork check: B (joining an already-settled covenant) must have chained off
    // A's REAL on-chain tip, not forked off a stale mid-chain settlement. The old racy adoption
    // produced 18 confirmed settlements on a side-branch with count_b=0 on the selected chain; this
    // asserts every settlement of the covenant in the DAG is on the single continuation chain.
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    l1.shutdown().await;
}

/// A settlement-mode prover RESUMES after it has already settled: it is restarted (a fresh, EMPTY
/// store + a fresh settler) from the SAME bootstrap `CovenantState` it deployed with, but the
/// bootstrap UTXO is now SPENT by the settlement it landed before the restart. This mirrors the
/// daemon reading covenant_id + bootstrap_block_hash from its state file and re-spawning the
/// settler: the settler reconstructs the bootstrap outpoint, finds it spent, and must adopt the
/// on-chain tip instead of panicking. Old behavior panicked at the startup confirm; the fix times
/// out and adopts.
#[tokio::test(flavor = "multi_thread")]
async fn prover_resumes_after_settlement() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping prover_resumes_after_settlement: RISC0_DEV_MODE!=1 - resume runs dev stub \
             proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.toccata_activation = ForkActivation::always();
            p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key);
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let bootstrap_outpoint = TransactionOutpoint::new(boot_txid, 0);
    // The bootstrap state the daemon would reconstruct from its persisted state file on restart:
    // the same covenant id + bootstrap outpoint + bootstrap SPK, replayed forward from the deploy.
    let bootstrap_state = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
        spk: bootstrap_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };

    let kp_a = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a = prover_address(&kp_a, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);

    // === Run 1: A lands at least one settlement, spending the bootstrap UTXO ===
    let prover_a1 = spawn_prover(
        &l1,
        "A1",
        kp_a,
        addr_a.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state.clone(),
        elfs,
        None,
        Some(block_deploy),
    )
    .await;

    for i in 0..10 {
        drive_range(&l1).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("resume run1 driver: iteration {i}, covenant chain length {len}");
        if len >= 1 {
            break;
        }
    }
    let chain_after_run1 =
        covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert!(
        chain_after_run1 >= 1,
        "prover A must land >=1 settlement (spending the bootstrap) before the restart, got {chain_after_run1}",
    );

    // Shut A down, mirroring a daemon stop. Its store is reclaimed; the next run starts EMPTY.
    prover_a1.shutdown.open();
    let join_a1 = prover_a1.settler.await;
    prover_a1.node.shutdown();
    assert!(join_a1.is_ok(), "prover A run 1 settler panicked: {join_a1:?}");

    // === Run 2: A RESUMES from the SAME bootstrap state, bootstrap now spent ===
    // Fresh store, fresh settler, same `CovenantState` the persisted state file would rebuild. The
    // settler reconstructs the spent bootstrap outpoint, times the confirm out, and adopts the
    // on-chain tip rather than panicking.
    let kp_a2 = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a2 = prover_address(&kp_a2, network_id);
    l1.fund_address(&addr_a2, FUND_VALUE, FUND_COUNT).await;
    let prover_a2 = spawn_prover(
        &l1,
        "A2",
        kp_a2,
        addr_a2.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state,
        elfs,
        None,
        Some(block_deploy),
    )
    .await;

    const DRIVER_ITERS: usize = 9;
    for i in 0..DRIVER_ITERS {
        eprintln!("resume run2 driver: iteration {i}");
        drive_range(&l1).await;
    }

    let mut prev_len = 0usize;
    let mut stable_rounds = 0;
    for _ in 0..40 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if len == prev_len {
            stable_rounds += 1;
        } else {
            stable_rounds = 0;
        }
        prev_len = len;
        if stable_rounds >= 3 && len > chain_after_run1 {
            break;
        }
    }

    prover_a2.shutdown.open();
    let join_a2 = prover_a2.settler.await;
    prover_a2.node.shutdown();

    // === Assertion 1: the resumed settler did NOT panic on the spent bootstrap ===
    assert!(
        join_a2.is_ok(),
        "resumed prover A2 settler panicked on a spent bootstrap (resume regression): {join_a2:?}",
    );

    // === Assertion 2: single contiguous chain that advanced past run 1 ===
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let mut expected_input = bootstrap_outpoint;
    for (pos, link) in chain.iter().enumerate() {
        assert_eq!(
            link.covenant_input, expected_input,
            "settlement #{pos} ({}) must spend the previous covenant output {expected_input}; the \
             continuation chain forked",
            link.tx_id,
        );
        expected_input = TransactionOutpoint::new(link.tx_id, 0);
    }
    assert!(
        chain.len() > chain_after_run1,
        "the resumed prover must advance the covenant past run 1 ({} <= {chain_after_run1})",
        chain.len(),
    );

    // The resumed settler must have chained off the on-chain tip it resolved, not forked.
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    l1.shutdown().await;
}

/// A CONTENDED resume: two provers run together against the unspent bootstrap, then ONE is
/// restarted from a fresh empty store mid-run, after the bootstrap has been spent. The restarted
/// prover must resolve the on-chain tip from the replay (its supplied bootstrap outpoint is spent),
/// rejoin the still-running competitor, and keep advancing the SAME single contiguous chain. This
/// proves the resume fix under contention, not just solo: the restarted settler lands on the live
/// tip the competitor advanced rather than forking off a stale point.
#[tokio::test(flavor = "multi_thread")]
async fn prover_resumes_after_settlement_contended() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping prover_resumes_after_settlement_contended: RISC0_DEV_MODE!=1 - resume runs \
             dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.toccata_activation = ForkActivation::always();
            p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key);
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
    eprintln!("dev covenant bootstrapped: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let bootstrap_outpoint = TransactionOutpoint::new(boot_txid, 0);
    let bootstrap_state = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
        spk: bootstrap_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };

    let kp_a = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let kp_b = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a = prover_address(&kp_a, network_id);
    let addr_b = prover_address(&kp_b, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;
    l1.fund_address(&addr_b, FUND_VALUE, FUND_COUNT).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);
    let pacer = Arc::new(AlternationPacer::new());

    // === Run 1: A and B contend from the unspent bootstrap (like two_provers_contend) ===
    // Both fresh-deploy provers seed from the deploy block, matching the binary's fresh-deploy path
    // (the live settlement handle is empty at startup, so each starts from the unspent bootstrap
    // and the loop's mid-stream adoption advances it as the bridge publishes settlements).
    let prover_a = spawn_prover(
        &l1,
        "A",
        kp_a,
        addr_a.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state.clone(),
        elfs,
        Some((0, pacer.clone())),
        Some(block_deploy),
    )
    .await;
    let prover_b1 = spawn_prover(
        &l1,
        "B1",
        kp_b,
        addr_b.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state.clone(),
        elfs,
        Some((1, pacer.clone())),
        Some(block_deploy),
    )
    .await;

    // Drive until the chain has advanced a few settlements, so the bootstrap is well spent before B
    // restarts.
    for i in 0..10 {
        drive_range(&l1).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("contended-resume run1 driver: iteration {i}, covenant chain length {len}");
        if len >= 2 {
            break;
        }
    }
    let chain_before_restart =
        covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert!(
        chain_before_restart >= 2,
        "the two provers must land >=2 settlements before B restarts, got {chain_before_restart}",
    );

    // === Restart B mid-run: shut B1 down, spawn B2 from a fresh empty store ===
    // B2 is handed the SAME bootstrap state the persisted state file would rebuild, but the
    // bootstrap UTXO is now spent. It seeds its bridge at the deploy block (start_from), replays L1
    // forward, and its settler resolves the on-chain tip A advanced rather than confirming the
    // spent bootstrap. A keeps running, so B2 rejoins under contention.
    prover_b1.shutdown.open();
    let join_b1 = prover_b1.settler.await;
    prover_b1.node.shutdown();
    assert!(join_b1.is_ok(), "prover B1 settler panicked: {join_b1:?}");

    let kp_b2 = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_b2 = prover_address(&kp_b2, network_id);
    l1.fund_address(&addr_b2, FUND_VALUE, FUND_COUNT).await;
    let prover_b2 = spawn_prover(
        &l1,
        "B2",
        kp_b2,
        addr_b2.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state,
        elfs,
        Some((1, pacer.clone())),
        Some(block_deploy),
    )
    .await;

    // === Run 2: A and the restarted B2 contend; drive then drain ===
    const DRIVER_ITERS: usize = 10;
    for i in 0..DRIVER_ITERS {
        eprintln!("contended-resume run2 driver: iteration {i}");
        drive_range(&l1).await;
    }

    // Drain: keep offering fresh ranges (not just acceptance) so a transiently-stalled contention
    // gets a new range to settle and cannot wedge the chain below the target under load.
    let mut prev_len = 0usize;
    let mut stable_rounds = 0;
    for _ in 0..40 {
        drive_range(&l1).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if len == prev_len {
            stable_rounds += 1;
        } else {
            stable_rounds = 0;
        }
        prev_len = len;
        if stable_rounds >= 3 && len > chain_before_restart {
            break;
        }
    }

    prover_a.shutdown.open();
    prover_b2.shutdown.open();
    let join_a = prover_a.settler.await;
    let join_b2 = prover_b2.settler.await;
    prover_a.node.shutdown();
    prover_b2.node.shutdown();

    // === Assertion 1: neither settler panicked (the resumed B2 on a spent bootstrap especially)
    // ===
    assert!(join_a.is_ok(), "prover A settler panicked: {join_a:?}");
    assert!(
        join_b2.is_ok(),
        "restarted prover B2 settler panicked on a spent bootstrap under contention: {join_b2:?}",
    );

    // === Assertion 2 + 3: single contiguous chain, advanced past the restart, both attributed ===
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let change_spk_a = pay_to_address_script(&addr_a);
    let change_spk_b1 = pay_to_address_script(&addr_b);
    let change_spk_b2 = pay_to_address_script(&addr_b2);

    let mut count_a = 0usize;
    // B-family: settlements from B's original key OR its restarted key, so the restart is
    // attributed to the same prover.
    let mut count_b = 0usize;
    let mut expected_input = bootstrap_outpoint;
    for (pos, link) in chain.iter().enumerate() {
        if link.change_spks.contains(&change_spk_a) {
            count_a += 1;
        }
        if link.change_spks.contains(&change_spk_b1) || link.change_spks.contains(&change_spk_b2) {
            count_b += 1;
        }
        assert_eq!(
            link.covenant_input, expected_input,
            "settlement #{pos} ({}) must spend the previous covenant output {expected_input}; the \
             continuation chain forked",
            link.tx_id,
        );
        expected_input = TransactionOutpoint::new(link.tx_id, 0);
    }

    assert!(
        chain.len() > chain_before_restart,
        "the chain must advance past the pre-restart settlements ({} <= {chain_before_restart})",
        chain.len(),
    );
    assert!(count_a >= 1, "prover A (addr {addr_a}) produced no settlements");
    assert!(
        count_b >= 1,
        "prover B (addrs {addr_b}/{addr_b2}) produced no settlements across the restart",
    );

    // No settlement landed on a DAG side-branch across the restart.
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    l1.shutdown().await;
}

/// Lane carriers mined per settlement range in the catch-up test (the bundle minimum).
const CATCHUP_CARRIERS: usize = 2;

/// Mines one settlement range for the catch-up test: a bundle's worth of lane carriers, then
/// several acceptance blocks (with pauses) so pending settlements land and confirm on chain.
async fn drive_range(l1: &L1Node) {
    for _ in 0..CATCHUP_CARRIERS {
        let payload =
            encode_activity_payload(&[AccessMetadata::write(ResourceId::for_test(1))], &[1, 2, 3]);
        let carrier = l1
            .build_subnet_payload_transactions(vec![payload], LANE_SUBNET, TX_VERSION_TOCCATA)
            .await
            .into_iter()
            .next()
            .expect("carrier tx");
        l1.mine_block(std::slice::from_ref(&carrier)).await;
    }
    for _ in 0..5 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// One link in the covenant continuation chain: a settlement tx, the covenant outpoint its input 0
/// spends, and the SPKs of all its outputs (so attribution can match a prover's change SPK).
struct CovenantLink {
    /// The settlement transaction's id.
    tx_id: Hash,
    /// Covenant outpoint this settlement's input 0 spends.
    covenant_input: TransactionOutpoint,
    /// SPKs of all outputs, used to attribute the settlement to a prover by its change SPK.
    change_spks: Vec<kaspa_consensus_core::tx::ScriptPublicKey>,
}

/// Derives a prover's P2PK funding address from its keypair under the network's prefix, matching
/// how the wallet / settler derive the address they fund fees from.
fn prover_address(keypair: &Keypair, network_id: NetworkId) -> Address {
    let (xonly, _parity) = keypair.x_only_public_key();
    Address::new(Prefix::from(network_id.network_type()), Version::PubKey, &xonly.serialize())
}

/// Builds, wires, and starts one prover: a proving [`RunnerNode`] over a fresh store + wRPC client,
/// and a spawned settler draining the node's settlement queue against the shared covenant. Returns
/// the assembled [`Prover`] (node kept alive, settler handle + shutdown latch for teardown).
#[allow(clippy::too_many_arguments)]
async fn spawn_prover(
    l1: &L1Node,
    label: &'static str,
    keypair: Keypair,
    address: Address,
    bundle_size: RangeInclusive<usize>,
    network_id: NetworkId,
    params: &Params,
    lane_key: Hash,
    covenant_id: Hash,
    covenant: CovenantState,
    elfs: Elfs<'_>,
    alternation: Option<(u8, Arc<AlternationPacer>)>,
    start_from: Option<Hash>,
) -> Prover {
    let wrpc_url = l1.wrpc_borsh_url();
    // Separate wRPC clients for the lane source and the settler so they own independent handles.
    let client_for_lane = connect_wrpc(&wrpc_url, network_id).await;
    let client_for_settler = connect_wrpc(&wrpc_url, network_id).await;

    let db_dir = TempDir::new().expect("temp dir");
    let store = RunnerStore::open(db_dir.path());

    let queue = SettlementQueue::new();
    // Each prover follows the same L1 through its own bridge, so each gets its own live settlement
    // channel: the bridge (writer) publishes settlements it observes (including the competitor's),
    // and this prover's settler (reader) reconciles against them.
    let (settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    let node = build_proving_node(
        elfs,
        store,
        BridgeParams {
            url: wrpc_url,
            network_id,
            lane_subnet: LANE_SUBNET,
            covenant_id,
            finality_depth: LANE_FINALITY_DEPTH,
            seed_depth: 0,
            start_from,
            observers: BridgeObservers { tip_daa: None, settlement: Some(settlement_tx) },
        },
        ProvingParams {
            covenant_id,
            // The counter `transaction-processor` credits no L1 deposits, so every batch carries
            // the no-deposit sentinel.
            deposit_spk_hash: [0u8; 32],
            lane_key,
            client: client_for_lane,
            sink: queue.clone(),
            bundle_size,
            // The aggregate prover re-forms a superseded suffix off the same settlement watch the
            // settler reconciles against, so a bundle a shorter competitor superseded still
            // settles.
            settlement_rx: Some(settlement_rx.clone()),
        },
    );

    // Backend is required by the settler config; in Dev mode it pins no image ids but the type is
    // still threaded through.
    let backend = Backend::new(elfs.program, elfs.batch, elfs.aggregator, ProofType::Succinct);
    let shutdown = AtomicAsyncLatch::new();
    let settler = tokio::spawn(run_settlement_worker(
        queue,
        SettlementWorkerConfig {
            client: client_for_settler,
            params: params.clone(),
            keypair,
            lane_key,
            covenant_id,
            start_from,
            backend,
            mode: SettlementMode::Dev,
            settlement: settlement_rx,
            // Jitter each submission so neither prover deterministically wins the spend race for
            // every range; without it the first-spawned prover lands every settlement. The window
            // is wide relative to the dev proving time so the per-range winner is a
            // genuine coin flip.
            submit_jitter: Some(0..40),
            // Strictly alternate with the competing prover: after one lands a settlement it waits
            // for the other to land the next, so neither sweeps every range (and each settles at
            // half rate, letting its recycled fee-change UTXO confirm before reuse). This makes the
            // both-provers-settled assertion deterministic rather than a coin flip per range.
            // `None` lets a solo prover settle every range without waiting on a partner.
            alternation,
        },
        covenant,
        shutdown.clone(),
    ));

    eprintln!("prover {label} started (addr {address})");
    Prover { node, settler, shutdown, _db_dir: db_dir }
}

/// Walks the selected-parent chain from the virtual tip down to `block_deploy`, collecting the
/// covenant continuation chain: the linear sequence of settlement txs that spend the bootstrap
/// outpoint and each other's output 0. Returns the links in chain order (bootstrap-spend first).
///
/// The driver is the only miner, so every block builds on the chain tip and the selected-parent
/// walk covers every settlement. We index settlements by the covenant outpoint they spend, then
/// thread from `bootstrap_outpoint` forward, which yields a single linear chain when (and only
/// when) the settlements never diverge.
async fn covenant_chain(
    l1: &L1Node,
    block_deploy: Hash,
    bootstrap_outpoint: TransactionOutpoint,
    covenant_id: Hash,
) -> Vec<CovenantLink> {
    // Map: covenant outpoint spent -> the settlement link that spends it.
    let mut by_input: HashMap<TransactionOutpoint, CovenantLink> = HashMap::new();

    // Walk selected parents from the virtual tip back to (and including) block_deploy.
    let tip = l1.grpc_client().get_block_dag_info().await.expect("dag info").sink;
    let mut cursor = tip;
    loop {
        let block = l1.grpc_client().get_block(cursor, true).await.expect("get_block");
        for rpc_tx in &block.transactions {
            let tx = match Transaction::try_from(rpc_tx.clone()) {
                Ok(tx) => tx,
                Err(_) => continue,
            };
            // A settlement of this covenant binds output 0 to it and ends input 0 with the
            // settlement tail; `settlement_info` returns Some only for those.
            // Only the structural match matters here, so the daa_score argument is irrelevant.
            if tx.settlement_info(covenant_id, cursor, 0).is_none() {
                continue;
            }
            let covenant_input = tx.inputs.first().expect("settlement input").previous_outpoint;
            let change_spks =
                tx.outputs.iter().map(|o| o.script_public_key.clone()).collect::<Vec<_>>();
            by_input.insert(
                covenant_input,
                CovenantLink { tx_id: tx.id(), covenant_input, change_spks },
            );
        }
        if cursor == block_deploy {
            break;
        }
        let selected_parent =
            block.verbose_data.as_ref().expect("verbose data").selected_parent_hash;
        cursor = selected_parent;
    }

    // Thread the chain from the bootstrap outpoint forward.
    let mut chain = Vec::new();
    let mut next_input = bootstrap_outpoint;
    while let Some(link) = by_input.remove(&next_input) {
        next_input = TransactionOutpoint::new(link.tx_id, 0);
        chain.push(link);
    }
    chain
}

/// Counts the DISTINCT settlement transactions of `covenant_id` across the WHOLE DAG above
/// `block_deploy` (every block, not just the selected-parent chain), deduplicated by txid.
///
/// A settlement that landed on a DAG SIDE-branch (a fork) is a settlement that exists in the DAG
/// but not on the selected-parent chain `covenant_chain` walks. Comparing this DAG-wide count to
/// the selected-chain length is the anti-fork check: they are equal iff no settlement of this
/// covenant ever forked off the single continuation chain.
async fn dag_settlement_count(l1: &L1Node, block_deploy: Hash, covenant_id: Hash) -> usize {
    // `get_blocks` is server-capped at roughly `mergeset_size_limit + 1` blocks per call, so a DAG
    // larger than one page truncates. Page forward: advance `low_hash` to the highest block of each
    // page and re-query until a page returns no block past the cursor (only the cursor echoes
    // back), deduping blocks by hash across the overlapping page boundaries.
    let mut settlement_txids = std::collections::HashSet::new();
    let mut seen_blocks = std::collections::HashSet::new();
    let mut low_hash = block_deploy;
    loop {
        let response = l1
            .grpc_client()
            .get_blocks(Some(low_hash), true, true)
            .await
            .expect("get_blocks from deploy");

        let mut highest = low_hash;
        let mut progressed = false;
        for block in &response.blocks {
            let block_hash = block.header.hash;
            // Page N's low_hash block re-appears as page N+1's first block; skip already-counted
            // blocks so the dedup is by block, not just by settlement txid.
            if !seen_blocks.insert(block_hash) {
                continue;
            }
            progressed = true;
            highest = block_hash;
            for rpc_tx in &block.transactions {
                let tx = match Transaction::try_from(rpc_tx.clone()) {
                    Ok(tx) => tx,
                    Err(_) => continue,
                };
                if tx.settlement_info(covenant_id, block_hash, 0).is_some() {
                    settlement_txids.insert(tx.id());
                }
            }
        }

        // No block past the cursor: the whole DAG above `block_deploy` has been read.
        if !progressed {
            break;
        }
        low_hash = highest;
    }
    settlement_txids.len()
}

/// Asserts the covenant never forked: every settlement on the selected-parent chain is reachable
/// from `bootstrap_outpoint` (already checked by the contiguity walk at the call site) AND the
/// count of this covenant's settlement txs ON the selected chain equals the total count of its
/// settlement txs in the DAG. A side-branch settlement (the fork failure mode the racy mid-loop
/// adoption produced) shows up as a DAG count strictly greater than the selected-chain length.
async fn assert_no_fork(
    l1: &L1Node,
    block_deploy: Hash,
    bootstrap_outpoint: TransactionOutpoint,
    covenant_id: Hash,
) {
    let chain_len = covenant_chain(l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    let dag_len = dag_settlement_count(l1, block_deploy, covenant_id).await;
    assert_eq!(
        chain_len, dag_len,
        "covenant forked: {dag_len} settlement txs in the DAG but only {chain_len} on the \
         selected-parent continuation chain (a settlement landed on a side-branch)",
    );
}

/// Connects a Borsh wRPC client to `url`, mirroring `main.rs`'s client construction.
async fn connect_wrpc(url: &str, network_id: NetworkId) -> KaspaRpcClient {
    let client =
        KaspaRpcClient::new_with_args(WrpcEncoding::Borsh, Some(url), None, Some(network_id), None)
            .expect("create wRPC client");
    client
        .connect(Some(ConnectOptions {
            block_async_connect: true,
            connect_timeout: Some(Duration::from_millis(10_000)),
            ..Default::default()
        }))
        .await
        .expect("connect to node wRPC");
    client
}
