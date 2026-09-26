//! End-to-end two-prover contention test (dev mode, CPU).
//!
//! Two independent prover nodes settle the SAME dev covenant against ONE simnet L1, racing from
//! different fee addresses with different bundle-size ranges, advancing each other. This proves the
//! settler's competitor-reconciliation resilience: under contention neither worker panics, both
//! provers land settlements, and the covenant continuation chain stays a single contiguous
//! spend-chain (the two provers cooperatively advance ONE covenant rather than forking it).
//!
//! The file also hosts the companion scenario tests sharing the same harness: catch-up joins,
//! resumes after settlement, and the single-prover live-lane join (`prover_joins_live_lane`) that
//! guards the bridge's authoritative lane-tip seeding at a fresh anchor.
//!
//! Runs only under `RISC0_DEV_MODE=1` (dev stub proofs + dev redeem; no GPU). The production /
//! CUDA path is covered by `zk/backend/risc0/test-suite/tests/settlement_l1_e2e.rs`.

use std::{collections::HashMap, ops::RangeInclusive, sync::Arc, time::Duration};

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{
    config::params::Params,
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
use vprogs_state_settlement_journal::{SettlementJournal, StoreJournal};
use vprogs_storage_types::{StateSpace, Store};
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
    /// Scratch dir backing the prover's RocksDB store in the temporary case: held for the run,
    /// then reclaimed on drop after the node is shut down so the store has already closed its
    /// files. `None` for a prover spawned over a caller-owned dir, whose lifetime the caller
    /// manages itself.
    _db_dir: Option<TempDir>,
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
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    // Coinbase feeds the bootstrap, both funding txs, and ongoing carrier fees; mine generously.
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the shared dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
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
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the shared dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
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
    // Settlements the contended ranges must land before the drain begins.
    const MIN_PRE_DRAIN_SETTLEMENTS: usize = 3;
    // Iteration ceiling. Dev proving runs on the CPU and trails the driver on a loaded host, so
    // past `DRIVER_ITERS` the loop keeps driving ranges until the chain catches up.
    const MAX_DRIVER_ITERS: usize = 40;
    let mut pre_drain_len = 0;
    for i in 0..MAX_DRIVER_ITERS {
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

        // Settlements landed so far; suffixes proved past this length may still be in flight,
        // superseded and awaiting re-aggregation against the adopted tip.
        pre_drain_len =
            covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if i % 4 == 0 {
            eprintln!(
                "reform driver: iteration {i}/{MAX_DRIVER_ITERS}, covenant chain length \
                 {pre_drain_len}",
            );
        }
        if i + 1 >= DRIVER_ITERS && pre_drain_len >= MIN_PRE_DRAIN_SETTLEMENTS {
            break;
        }
    }
    assert!(
        pre_drain_len >= MIN_PRE_DRAIN_SETTLEMENTS,
        "the two provers must land >={MIN_PRE_DRAIN_SETTLEMENTS} settlements under contention \
         before the drain, got {pre_drain_len}",
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
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
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

    let (_redeem, catchup_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
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
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
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
    let (_redeem, catchup_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
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
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
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
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
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
    // gets a new range to settle and cannot wedge the chain below the target under load. The
    // deadline, not a round budget, bounds the drain: a slow machine needs more rounds than any
    // fixed count, while a genuinely starved drain still surfaces in the chain-advance assert
    // below once the deadline passes.
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(300);
    let mut prev_len = 0usize;
    let mut stable_rounds = 0;
    while std::time::Instant::now() < drain_deadline {
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

/// A warm-restart settlement resume, the TN5 incident shape: run 1 settles a few ranges over a
/// PERSISTENT store, then proves one more bundle whose settlement is submitted but left
/// UNCONFIRMED (no block mines after its submission), and the prover is killed mid confirm-wait.
/// Run 2 reopens the same store: the journaled bundle survives the restart, is re-fed from the
/// journal, and settles once acceptance blocks mine. Asserts the chain advanced past run 1's
/// settled count with no fork, and that run 1's journal entries compacted away after the tail
/// landed.
#[tokio::test(flavor = "multi_thread")]
async fn warm_restart_settles_pending_tail() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping warm_restart_settles_pending_tail: RISC0_DEV_MODE!=1 - the warm restart \
             runs dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
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
    let addr_a = prover_address(&kp_a, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);

    // === Run 1 over a persistent store: settle a few ranges, then leave the last proved bundle
    // unconfirmed (no block mines after its submission) and kill the prover mid confirm-wait. ===
    let db_dir = TempDir::new().expect("persistent store dir");
    let prover_a1 = spawn_prover_over_store(
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
        db_dir.path(),
    )
    .await;

    let mut settled;
    for i in 0..10 {
        drive_range(&l1).await;
        settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("warm-restart run1 driver: iteration {i}, covenant chain length {settled}");
        if settled >= 2 {
            break;
        }
    }
    // Land any straggler settlement before building the pending tail: the tail construction
    // below keys off the chain tip, which must be the LAST settlement for its spender poll to
    // catch exactly the new bundle's submission.
    await_mempool_settlement_free(&l1, covenant_id).await;
    settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert!(settled >= 2, "run 1 must land >=2 settlements before the stall, got {settled}");

    // One bundle's worth of carriers with NO acceptance blocks: the pipeline proves the next
    // bundle, journals it, and submits its settlement, which then sits in the mempool. The
    // mempool observation proves the bundle reached submission, so its journal entry exists.
    mine_lane_carriers(&l1, CATCHUP_CARRIERS).await;
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let tip_outpoint = TransactionOutpoint::new(chain.last().expect("run 1 settled tip").tx_id, 0);
    await_mempool_spender(&l1, tip_outpoint).await;
    // Settle before the kill: the tail's batch COMMITS ride the storage write worker's batched
    // queue, and a shutdown discards its unflushed batch, so give the queue time to land the
    // commits whose checkpoint ids the journal entry and the resumed store's compaction both key
    // on. Losing them would let run 2's fresh batches reuse the ids for different blocks.
    tokio::time::sleep(Duration::from_secs(2)).await;

    prover_a1.shutdown.open();
    let join_a1 = prover_a1.settler.await;
    prover_a1.node.shutdown();
    assert!(join_a1.is_ok(), "run 1 settler panicked: {join_a1:?}");

    // The store is released; reopen it and confirm the unconfirmed bundle survived the kill.
    // The highest journal index also fences run 1's entries from run 2's: the restarted store
    // continues checkpoint numbering upward, so any entry at or below this fence belongs to run
    // 1 and must compact away once the tail settles.
    let run1_fence = {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("warm-restart journal after run 1: bundle {start}..={}", entry.end_index);
        }
        assert!(
            !entries.is_empty(),
            "the unconfirmed tail bundle must be journaled across the kill",
        );
        entries.last().expect("checked non-empty").1.end_index
    };

    // === Run 2 over the SAME store: a fresh funded keypair, and the journaled tail re-feeds and
    // settles once acceptance blocks mine. ===
    let kp_a2 = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a2 = prover_address(&kp_a2, network_id);
    l1.fund_address(&addr_a2, FUND_VALUE, FUND_COUNT).await;
    let prover_a2 = spawn_prover_over_store(
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
        db_dir.path(),
    )
    .await;

    let mut final_len = 0usize;
    for round in 0..40 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        final_len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if round % 5 == 0 {
            eprintln!("warm-restart run2 drain: round {round}, covenant chain length {final_len}");
        }
        if final_len > settled {
            break;
        }
    }
    // Compaction is eventual and starts only after run 2's bridge connects and publishes its
    // baseline (seconds under load), the resume re-feeds the tail, and the settlement lands: keep
    // mining empty blocks (each forces a bridge publication, which re-runs compaction) well past
    // the observed growth before the store closes.
    for _ in 0..30 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    prover_a2.shutdown.open();
    let join_a2 = prover_a2.settler.await;
    prover_a2.node.shutdown();
    assert!(join_a2.is_ok(), "run 2 settler panicked: {join_a2:?}");

    assert!(final_len > settled, "resume must settle the pending tail, {settled} -> {final_len}",);
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    // Run 1's journal entries must have compacted away: the fence separates the killed run's
    // entries from run 2's, and the settled tail's own boundary maps inside its span, so its
    // deletion is deterministic. Entries ABOVE the fence are run 2's freshly-proved stragglers
    // over the acceptance-window blocks; the settler skips the ones whose state chain already
    // advanced, and such an entry can linger (a known leak shape), so only the fenced range is
    // asserted here.
    let compacted = {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("warm-restart journal after run 2: bundle {start}..={}", entry.end_index);
        }
        entries.iter().all(|(start, _)| *start > run1_fence)
    };
    assert!(
        compacted,
        "run 1's journal entries (<= {run1_fence}) must compact after the resumed tail settles",
    );

    l1.shutdown().await;
}

/// A warm-restart resume into a competitor sweep: run 1 leaves a pending tail over a persistent
/// store and dies; while it is down, a second prover with a fresh store sweeps the whole pending
/// range and keeps settling. Run 2 reopens the first store: its journal entries are covered by
/// the swept tip (compacted away, never re-settled), and the resumed prover keeps advancing the
/// SAME chain with new work. Asserts the chain advanced past the sweep with no fork and that
/// run 1's journal entries compacted away.
#[tokio::test(flavor = "multi_thread")]
async fn warm_restart_after_competitor_sweeps_tail() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping warm_restart_after_competitor_sweeps_tail: RISC0_DEV_MODE!=1 - the warm \
             restart runs dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
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

    // === Run 1 over a persistent store: settle a few ranges, then leave the pending tail. ===
    let db_dir = TempDir::new().expect("persistent store dir");
    let prover_a1 = spawn_prover_over_store(
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
        db_dir.path(),
    )
    .await;

    let mut settled;
    for i in 0..10 {
        drive_range(&l1).await;
        settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("sweep run1 driver: iteration {i}, covenant chain length {settled}");
        if settled >= 2 {
            break;
        }
    }
    // Land any straggler settlement before building the pending tail, so the tail's spender poll
    // catches exactly the new bundle's submission against the last landed tip.
    await_mempool_settlement_free(&l1, covenant_id).await;
    settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert!(settled >= 2, "run 1 must land >=2 settlements before the sweep, got {settled}");

    mine_lane_carriers(&l1, CATCHUP_CARRIERS).await;
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let tip_outpoint = TransactionOutpoint::new(chain.last().expect("run 1 settled tip").tx_id, 0);
    await_mempool_spender(&l1, tip_outpoint).await;
    // Settle before the kill, as in the pending-tail test: the tail's batch commits and the
    // bridge's processing of the carrier blocks must land in the store before it closes.
    tokio::time::sleep(Duration::from_secs(2)).await;

    prover_a1.shutdown.open();
    let join_a1 = prover_a1.settler.await;
    prover_a1.node.shutdown();
    assert!(join_a1.is_ok(), "run 1 settler panicked: {join_a1:?}");

    // The killed run's journal fence: the highest index run 1 journaled. Run 2 continues
    // checkpoint numbering upward, so entries at or below the fence are run 1's and must
    // compact away against the swept tip.
    let run1_fence = {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("sweep journal after run 1: bundle {start}..={}", entry.end_index);
        }
        assert!(!entries.is_empty(), "the pending tail bundle must be journaled across the kill",);
        entries.last().expect("checked non-empty").1.end_index
    };

    // === Competitor B (fresh store, its own settler) sweeps the whole pending range. Batches
    // settle in order, so any chain growth past run 1's tip implies the pending tail's range was
    // covered, whether by B's settlement or run 1's leftover mempool submission. ===
    let (_redeem, catchup_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
    let catchup_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
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

    let mut after_sweep = 0usize;
    for i in 0..10 {
        drive_range(&l1).await;
        after_sweep =
            covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("sweep driver: iteration {i}, covenant chain length {after_sweep}");
        if after_sweep >= settled + 2 {
            break;
        }
    }
    prover_b.shutdown.open();
    let join_b = prover_b.settler.await;
    prover_b.node.shutdown();
    assert!(join_b.is_ok(), "sweep prover B settler panicked: {join_b:?}");
    assert!(
        after_sweep > settled,
        "the competitor sweep must cover run 1's pending tail ({settled} -> {after_sweep})",
    );

    // === Run 2 over the SAME store: the swept entries compact against the tip, and the resumed
    // prover settles NEW work onto the competitor-advanced chain. ===
    let kp_a2 = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a2 = prover_address(&kp_a2, network_id);
    l1.fund_address(&addr_a2, FUND_VALUE, FUND_COUNT).await;
    let prover_a2 = spawn_prover_over_store(
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
        db_dir.path(),
    )
    .await;

    let mut final_len = 0usize;
    for i in 0..40 {
        drive_range(&l1).await;
        final_len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("sweep run2 driver: iteration {i}, covenant chain length {final_len}");
        if final_len > after_sweep {
            break;
        }
    }

    // Acceptance-only drain (no fresh carriers) so in-flight settlements land and compact before
    // the teardown, the same stabilization the sibling tests run before asserting. Compaction is
    // eventual: each observed settlement re-runs it on the next bridge publication, so the drain
    // keeps mining a few extra blocks past stability to give the last entries their compaction
    // pass before the store closes.
    let mut prev_len = 0usize;
    let mut stable_rounds = 0usize;
    for _ in 0..40 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        final_len = len;
        if len == prev_len {
            stable_rounds += 1;
        } else {
            stable_rounds = 0;
        }
        prev_len = len;
        if stable_rounds >= 3 {
            break;
        }
    }
    // Same eventual-compaction window as the pending-tail test: the resumed prover's bridge
    // connect, resume, and post-settlement compaction all need publications to progress.
    for _ in 0..30 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    prover_a2.shutdown.open();
    let join_a2 = prover_a2.settler.await;
    prover_a2.node.shutdown();
    assert!(join_a2.is_ok(), "sweep run 2 settler panicked: {join_a2:?}");

    assert!(
        final_len > after_sweep,
        "the resumed prover must settle new work after the sweep ({after_sweep} -> {final_len})",
    );
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    // Run 1's journal entries must have compacted away against the swept tip (same fence
    // rationale as the pending-tail test; run 2 stragglers above the fence are tolerated).
    let compacted = {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("sweep journal after run 2: bundle {start}..={}", entry.end_index);
        }
        entries.iter().all(|(start, _)| *start > run1_fence)
    };
    assert!(
        compacted,
        "run 1's journal entries (<= {run1_fence}) must compact against the swept tip",
    );

    l1.shutdown().await;
}

/// A warm-restart resume into a competitor split: run 1 (bundle size 4) proves at least two
/// pending bundles over a persistent store; its settler confirms one settlement at a time, so
/// only the first is submitted before the prover is killed, leaving the last (a 4-batch
/// straddler) journaled but never submitted. The submitted settlement is mined in alone, then a
/// per-batch competitor (bundle size 1) lands a settlement whose proving boundary falls STRICTLY
/// inside the straddler's span while the first prover stays down. Run 2 reopens the store: the
/// bridge's startup publication is the pre-downtime baseline, the competitor's interior boundary
/// reaches the watch only as a later advance, and the advance pass of the resume must SPLIT the
/// straddled entry at that boundary, re-aggregating the suffix from cached per-batch receipts.
/// NO new carriers are driven in run 2, so a chain advance past the competitor is precisely the
/// split suffix landing. Also asserts the straddle itself (the competitor's boundary maps
/// through run 1's own batch metadata into the entry's interior), the no-fork invariant, and
/// that the pre-restart journal scope compacted away.
#[tokio::test(flavor = "multi_thread")]
async fn warm_restart_splits_at_competitor_boundary() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping warm_restart_splits_at_competitor_boundary: RISC0_DEV_MODE!=1 - the warm \
             restart runs dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
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

    // === Run 1: settle one range, then prove several pending 4-batch bundles. The settler
    // submits only the first (it confirms one settlement at a time and nothing mines), so the
    // journal's last entry, the straddler, is never submitted. ===
    let db_dir = TempDir::new().expect("persistent store dir");
    let prover_a1 = spawn_prover_over_store(
        &l1,
        "A1",
        kp_a,
        addr_a.clone(),
        4..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state.clone(),
        elfs,
        None,
        Some(block_deploy),
        db_dir.path(),
    )
    .await;

    let mut settled;
    for i in 0..10 {
        drive_range(&l1).await;
        settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("split run1 driver: iteration {i}, covenant chain length {settled}");
        if settled >= 1 {
            break;
        }
    }
    // Land any straggler settlement before building the straddler, as in the other warm-restart
    // tests, so the pending-tail construction keys off the last landed tip.
    await_mempool_settlement_free(&l1, covenant_id).await;
    settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert!(settled >= 1, "run 1 must land >=1 settlement before the split setup, got {settled}");

    // Eight carriers form at least two 4-batch bundles past whatever residual batches the drive
    // loop left, so the journal holds a submitted predecessor plus the unsubmitted straddler.
    mine_lane_carriers(&l1, 8).await;
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let tip_outpoint = TransactionOutpoint::new(chain.last().expect("run 1 settled tip").tx_id, 0);
    await_mempool_spender(&l1, tip_outpoint).await;
    // Drain before the kill: the eight-carrier wave leaves a backlog of batch lifecycles plus
    // the straddler's own proof in flight, and the kill must land on an idle pipeline so its
    // aggregate receipt write has flushed (a write queued when the node shuts down is dropped by
    // the storage write worker, wedging the proving worker on the write's confirmation latch and
    // leaking the store lock) and the straddler's batch commits are durable for the restart's
    // boundary mapping.
    tokio::time::sleep(Duration::from_secs(6)).await;

    prover_a1.shutdown.open();
    let join_a1 = prover_a1.settler.await;
    prover_a1.node.shutdown();
    assert!(join_a1.is_ok(), "split run 1 settler panicked: {join_a1:?}");
    // The eight-carrier wave leaves a backlog of batch lifecycles mid-flight at the kill, and
    // the scheduler's execution workers trail the shutdown for seconds; let them finish so the
    // store lock is released before reopening.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The straddler is the journal's last entry: a 4-batch bundle whose interior checkpoints a
    // per-batch competitor can settle into. The blocks of every interior checkpoint drive the
    // competitor's kill signal below.
    let (straddle_start, straddle_end, interior_blocks) = {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("split journal after run 1: bundle {start}..={}", entry.end_index);
        }
        assert!(
            entries.len() >= 2,
            "run 1 must journal a submitted predecessor plus the straddler",
        );
        let (start, entry) = *entries.last().expect("checked non-empty");
        assert_eq!(
            entry.end_index,
            start + 3,
            "the straddler must be one 4-batch bundle, got {start}..={}",
            entry.end_index,
        );
        let blocks =
            (start..entry.end_index).filter_map(|i| journal.batch_block(i)).collect::<Vec<_>>();
        (start, entry.end_index, blocks)
    };
    assert_eq!(
        interior_blocks.len(),
        (straddle_end - straddle_start) as usize,
        "every interior checkpoint must have batch metadata",
    );

    // === Land run 1's one submitted settlement alone: it is the only settlement in the mempool,
    // so a single mined block confirms it deterministically. ===
    let pre_land = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    l1.mine_blocks(1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after_first =
        covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert_eq!(
        after_first,
        pre_land + 1,
        "the single mined block must confirm run 1's one pending settlement",
    );

    // === Competitor B (bundle size 1) lands a settlement proving through an interior
    // checkpoint while A stays down. Its settler confirms one settlement at a time, so killing
    // it while its interior submission sits unconfirmed keeps its later bundles unsubmitted; the
    // poll interleaves sparse mining so B's bridge pages its replay forward. ===
    let (_redeem, catchup_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
    let catchup_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
        spk: catchup_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };
    let prover_b = spawn_prover(
        &l1,
        "B",
        kp_b,
        addr_b.clone(),
        1..=1,
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
    await_mempool_proving_through(&l1, covenant_id, &interior_blocks).await;
    prover_b.shutdown.open();
    let join_b = prover_b.settler.await;
    prover_b.node.shutdown();
    assert!(join_b.is_ok(), "split competitor B settler panicked: {join_b:?}");

    // Land whatever B submitted (its interior settlement, at most the few that sneaked into the
    // poll loop's sparse blocks), then pin the competitor's boundary on chain.
    let mut after_b = after_first;
    for round in 0..40 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        after_b = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if round % 5 == 0 {
            eprintln!("split competitor drain: round {round}, covenant chain length {after_b}");
        }
        if after_b > after_first {
            break;
        }
    }
    assert!(
        after_b > after_first,
        "the competitor's interior settlement must land past run 1's tail",
    );
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let b_link = chain.last().expect("competitor settlement link");
    assert!(
        b_link.change_spks.contains(&pay_to_address_script(&addr_b)),
        "the interior settlement must attribute to the competitor",
    );

    // The straddle itself: the competitor's boundary maps, through run 1's own batch metadata,
    // to a checkpoint strictly inside the straddler's span.
    let boundary = {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        journal
            .checkpoint_of_block(b_link.block_prove_to, straddle_end, straddle_start)
            .expect("the competitor's boundary must map into the straddler's span")
    };
    assert!(
        straddle_start <= boundary && boundary < straddle_end,
        "the competitor's boundary {boundary} must fall strictly inside the straddled entry \
         {straddle_start}..={straddle_end}",
    );

    // === Run 2 over the SAME store, driving NO new carriers: the only settleable work is the
    // straddled entry's suffix, so a chain advance past the competitor is exactly the advance
    // pass splitting the entry at the competitor's boundary, re-aggregating the suffix from
    // cached per-batch receipts, and landing it. The test keeps a clone of the store handle so
    // the compaction wait below can read the journal while the node runs. ===
    let kp_a2 = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a2 = prover_address(&kp_a2, network_id);
    l1.fund_address(&addr_a2, FUND_VALUE, FUND_COUNT).await;
    let store_a2 = open_store_retrying(db_dir.path());
    let prover_a2 = spawn_prover_on_store(
        &l1,
        "A2",
        kp_a2,
        addr_a2.clone(),
        4..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state,
        elfs,
        None,
        Some(block_deploy),
        store_a2.clone(),
        false,
    )
    .await;
    let journal_a2 = StoreJournal::new(store_a2.clone());

    let mut final_len = after_b;
    for round in 0..60 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        final_len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if round % 5 == 0 {
            eprintln!("split run2 drain: round {round}, covenant chain length {final_len}");
        }
        if final_len > after_b {
            break;
        }
    }
    // Compaction window: the split successor's entry deletes on a later publication, and
    // compaction only reruns once the bridge observes the covering settlement, so a fixed block
    // budget can expire before the compaction pass on a slow machine. Wait for the deletion
    // itself under a deadline while the node still runs, mining a block each round to force the
    // publication that reruns compaction.
    let compaction_deadline = std::time::Instant::now() + Duration::from_secs(300);
    while std::time::Instant::now() < compaction_deadline {
        if journal_a2.entries().iter().all(|(_, entry)| entry.end_index > straddle_end) {
            break;
        }
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    prover_a2.shutdown.open();
    let join_a2 = prover_a2.settler.await;
    prover_a2.node.shutdown();
    assert!(join_a2.is_ok(), "split run 2 settler panicked: {join_a2:?}");

    assert!(
        final_len > after_b,
        "the split suffix must settle past the competitor's boundary ({after_b} -> {final_len}); \
         no other work exists to advance the chain",
    );
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let suffix_link = chain.last().expect("split suffix settlement link");
    assert!(
        suffix_link.change_spks.contains(&pay_to_address_script(&addr_a2)),
        "the post-split settlement must attribute to the resumed prover",
    );
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    // The pre-restart journal scope compacted away: the straddler and its split successor both
    // end at or below the straddler's end, so no snapshot-scoped entry may survive (run 2
    // stragglers above the scope are tolerated, as in the other warm-restart tests). The handle
    // is shared with the node, not reopened, so the read rides the same RocksDB lock.
    let compacted = {
        let entries = journal_a2.entries();
        for (start, entry) in &entries {
            eprintln!("split journal after run 2: bundle {start}..={}", entry.end_index);
        }
        entries.iter().all(|(_, entry)| entry.end_index > straddle_end)
    };
    assert!(
        compacted,
        "the pre-restart journal scope (<= {straddle_end}) must compact after the split suffix \
         settles",
    );

    l1.shutdown().await;
}

/// A warm-restart resume whose own pending settlement LANDED during the downtime: run 1 leaves a
/// settlement in the mempool and is killed; a mined block sweeps it in, so the chain has already
/// spent the covenant input the journal still lists as pending when run 2 restarts. The restarted
/// settler must derive settled-ness from the chain BEFORE submitting: its re-fed bundle is
/// reported superseded without a submission, no fork lands, and new work settles on top of the
/// landed settlement. Asserts the landed settlement is on the chain, no fatal settler stop, the
/// chain advances past it with new carriers, no fork, and that the pre-restart journal scope
/// compacted away.
#[tokio::test(flavor = "multi_thread")]
async fn warm_restart_after_own_settlement_lands() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping warm_restart_after_own_settlement_lands: RISC0_DEV_MODE!=1 - the warm \
             restart runs dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    // Extra wallet UTXOs over the sibling tests: this test's two runs drive many more carrier
    // ranges (the run-2 growth loop keeps driving until new work settles), and the L1 wallet
    // funds every carrier; late mined blocks carry no lane activity at all once the spendable
    // set thins out.
    l1.mine_utxos(90).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
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
    let addr_a = prover_address(&kp_a, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);

    // === Run 1: drive work until a settlement sits submitted but unconfirmed in the mempool,
    // and kill the prover. ===
    // Bundle minimum 1 (unlike the sibling warm-restart tests' 2..=4): a carrier block can
    // occasionally carry no lane tx (the L1 wallet's spendable set thins out late in the run), and
    // with a minimum of 2 an empty block pairing off with another empty one strands a lone real
    // batch below the bundle minimum forever, so the tail settlement never exists to await.
    let db_dir = TempDir::new().expect("persistent store dir");
    let prover_a1 = spawn_prover_over_store(
        &l1,
        "A1",
        kp_a,
        addr_a.clone(),
        1..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state.clone(),
        elfs,
        None,
        Some(block_deploy),
        db_dir.path(),
    )
    .await;

    let mut settled;
    for i in 0..10 {
        drive_range(&l1).await;
        settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("own-lands run1 driver: iteration {i}, covenant chain length {settled}");
        if settled >= 2 {
            break;
        }
    }
    // Quiesce the pipeline before building the tail: land every straggler settlement AND let the
    // aggregate worker finish every in-flight bundle, so no batch commits past the journal tail
    // across the kill. A batch committed but never bundled (a straggler drive-loop carrier caught
    // mid-pipeline) leaves the warm store's state chain past the last settlement with no journal
    // entry covering the gap, and the resumed bundles then prove from a state the covenant never
    // took, so the settler skips them forever. The mempool staying settlement-free across a quiet
    // window is the observable of that drained pipeline: every proved bundle submits.
    for round in 0..40 {
        await_mempool_settlement_free(&l1, covenant_id).await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if mempool_settlement_free(&l1, covenant_id).await {
            eprintln!("own-lands run1 quiesced (round {round})");
            break;
        }
    }
    settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert!(settled >= 2, "run 1 must land >=2 settlements before the stall, got {settled}");

    mine_lane_carriers(&l1, CATCHUP_CARRIERS).await;
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let tip_outpoint = TransactionOutpoint::new(chain.last().expect("run 1 settled tip").tx_id, 0);
    await_mempool_spender(&l1, tip_outpoint).await;
    // Drain before the kill so the pipeline is idle and its batch commits are durable.
    tokio::time::sleep(Duration::from_secs(2)).await;

    prover_a1.shutdown.open();
    let join_a1 = prover_a1.settler.await;
    prover_a1.node.shutdown();
    assert!(join_a1.is_ok(), "run 1 settler panicked: {join_a1:?}");
    // The kill leaves the scheduler's execution workers trailing the shutdown for seconds; let
    // them finish so the store lock is released before reopening (as in the sibling restart
    // tests).
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The journal still lists the submitted bundle as pending: the kill predates its landing.
    // The landing assert below pins that exactly ONE settlement was pending (a second mempool
    // settlement would land with it and grow the chain by two), so here only non-emptiness is
    // asserted: a landed-but-not-yet-compacted entry may also survive the kill, and the resume
    // deletes it against the baseline.
    let run1_fence = {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("own-lands journal after run 1: bundle {start}..={}", entry.end_index);
        }
        assert!(!entries.is_empty(), "the submitted tail bundle must be journaled across the kill",);
        entries.last().expect("checked non-empty").1.end_index
    };

    // === The downtime landing: one mined block sweeps the mempool, confirming run 1's pending
    // settlement (the only one in the mempool) while the prover is down. ===
    let pre_land = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    l1.mine_blocks(1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after_land = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
    assert_eq!(
        after_land,
        pre_land + 1,
        "the single mined block must confirm run 1's pending settlement during the downtime",
    );

    // === Run 2 over the SAME store: the journal lists the landed range as pending and the watch
    // baseline predates it, so the re-fed bundle reaches the settler. It must be superseded
    // WITHOUT a second submission spending the already-spent covenant input: with the watch
    // caught up to the landing the settler's base-mismatch guard skips it, and on a lagging
    // watch the pre-submit liveness probe reports the spent outpoint instead. The landed
    // settlement is recognized once replayed, and new work settles on top of it. ===
    let kp_a2 = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a2 = prover_address(&kp_a2, network_id);
    l1.fund_address(&addr_a2, FUND_VALUE, FUND_COUNT).await;
    let prover_a2 = spawn_prover_over_store(
        &l1,
        "A2",
        kp_a2,
        addr_a2.clone(),
        1..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state,
        elfs,
        None,
        Some(block_deploy),
        db_dir.path(),
    )
    .await;

    let mut final_len = after_land;
    for i in 0..20 {
        drive_range(&l1).await;
        final_len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("own-lands run2 driver: iteration {i}, covenant chain length {final_len}");
        if final_len > after_land {
            break;
        }
    }
    // Compaction window for the landed entry and any in-flight tail, as in the other tests.
    for _ in 0..30 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    prover_a2.shutdown.open();
    let join_a2 = prover_a2.settler.await;
    prover_a2.node.shutdown();
    // No fatal settler stop across the superseded re-feed.
    assert!(join_a2.is_ok(), "run 2 settler stopped on the landed own settlement: {join_a2:?}");

    assert!(
        final_len > after_land,
        "new work must settle on top of the landed settlement ({after_land} -> {final_len})",
    );
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    // Run 1's journal entries compacted away against the landed tip (same fence rationale as the
    // other warm-restart tests; run 2 stragglers above the fence are tolerated).
    let compacted = {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("own-lands journal after run 2: bundle {start}..={}", entry.end_index);
        }
        entries.iter().all(|(start, _)| *start > run1_fence)
    };
    assert!(
        compacted,
        "run 1's journal entries (<= {run1_fence}) must compact against the landed tip",
    );

    l1.shutdown().await;
}

/// A kill between a batch's commit and its bundle's journal record must not wedge the covenant:
/// run 1 leaves committed batches above the journal tail (their bundles never journaled) and is
/// killed without quiescing the tail; run 2's startup must re-form the committed range from the
/// cached per-batch receipts, settle it, and settle new work past it. Without the committed-gap
/// pass run 2 proves every new bundle from a state root the covenant never took, the settler
/// skips them forever, and the chain stalls with zero submissions (the restarted-prover-idle
/// signature).
#[tokio::test(flavor = "multi_thread")]
async fn warm_restart_covers_committed_gap() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping warm_restart_covers_committed_gap: RISC0_DEV_MODE!=1 - the warm restart \
             runs dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(90).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Bootstrap the dev covenant (same shape as the own-lands sibling) ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
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
    let addr_a = prover_address(&kp_a, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);

    // === Run 1: drive landed settlements, then park committed-but-unbundled tail batches and
    // kill mid-window. ===
    // Bundle minimum 4, above the two tail carriers: with mining held after the carriers, the
    // ready prefix stays below the minimum, so the tail batches commit (metadata + receipts
    // durable) while their bundle provably cannot journal before the kill. Leftover parked
    // batches from the drive loop may carry the prefix to the minimum when a carrier arrives;
    // the kill-point loop below detects that journal growth and retries on a fresh window.
    let db_dir = TempDir::new().expect("persistent store dir");
    let store = RunnerStore::open(db_dir.path());
    let prover_a1 = spawn_prover_on_store(
        &l1,
        "A1",
        kp_a,
        addr_a.clone(),
        4..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state.clone(),
        elfs,
        None,
        Some(block_deploy),
        store.clone(),
        false,
    )
    .await;

    let mut settled = 0usize;
    for i in 0..10 {
        drive_range(&l1).await;
        settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("committed-gap run1 driver: iteration {i}, covenant chain length {settled}");
        if settled >= 2 {
            break;
        }
    }
    assert!(settled >= 2, "run 1 must land >=2 settlements before the kill, got {settled}");

    // The deterministic kill point: land stragglers, snapshot the journal, mine the tail
    // carriers, then poll the store until the carriers' metadata and receipts are durable with
    // the journal unchanged. The sub-minimum park makes that state stable (no bundle can form),
    // so the poll cannot race a journal record.
    let journal = StoreJournal::new(store.clone());
    let mut kill = None;
    for round in 0..6 {
        await_mempool_settlement_free(&l1, covenant_id).await;
        tokio::time::sleep(Duration::from_millis(1000)).await;

        let journal_max = journal.entries().last().map(|(_, entry)| entry.end_index).unwrap_or(0);
        let Some((pre_carriers, _)) = journal.committed_tip() else {
            panic!("run 1 committed no batches");
        };
        let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
        let boundary = journal
            .checkpoint_of_block(
                chain.last().expect("run 1 settled tip").block_prove_to,
                pre_carriers,
                1,
            )
            .expect("the settled boundary maps into committed metadata");

        mine_lane_carriers(&l1, CATCHUP_CARRIERS).await;
        for _poll in 0..600 {
            let Some((committed, _)) = journal.committed_tip() else {
                panic!("store lost metadata")
            };
            if journal.entries().iter().any(|(_, entry)| entry.end_index > journal_max) {
                eprintln!(
                    "committed-gap kill-point round {round}: a parked bundle reached the \
                     minimum and journaled past {journal_max}; retrying on a fresh window",
                );
                break;
            }
            if committed >= pre_carriers + CATCHUP_CARRIERS as u64
                && gap_batches_ready(&journal, &store, boundary + 1, committed)
            {
                // Read the fence fresh at the decision instant: a straggler settlement may have
                // landed on a carrier block mid-poll, advancing the chain boundary and
                // compacting the journal past the round's snapshots. The gap's precondition
                // (committed strictly past both fences) must hold at that same instant; a
                // straggler that degraded the point cannot recover while mining is held, so the
                // round retries on a fresh window instead of failing the later assert.
                let chain =
                    covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
                let boundary_now = journal
                    .checkpoint_of_block(
                        chain.last().expect("run 1 settled tip").block_prove_to,
                        committed,
                        1,
                    )
                    .expect("the settled boundary maps into committed metadata");
                let journal_now =
                    journal.entries().last().map(|(_, entry)| entry.end_index).unwrap_or(0);
                if committed > boundary_now.max(journal_now) {
                    kill = Some((boundary_now, committed, journal_now));
                } else {
                    eprintln!(
                        "committed-gap kill-point round {round}: a straggler settlement degraded \
                         the point (boundary {boundary_now}, journal max {journal_now}, committed \
                         tip {committed}); retrying on a fresh window",
                    );
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if kill.is_some() {
            break;
        }
    }
    let Some((kill_boundary, kill_committed, kill_journal_max)) = kill else {
        panic!("no deterministic committed-but-unjournaled kill point reached");
    };
    eprintln!(
        "committed-gap kill point: boundary {kill_boundary}, committed tip {kill_committed}, \
         journal max {kill_journal_max}",
    );
    // The defect's precondition: committed batches above both the journal tail and the on-chain
    // boundary, at least one carrying real lane work for the re-formation to recover.
    assert!(
        kill_committed > kill_boundary.max(kill_journal_max),
        "the kill point must hold committed batches past the journal tail {kill_journal_max} and \
         boundary {kill_boundary}, got committed tip {kill_committed}",
    );
    assert!(
        gap_real_batches(&journal, kill_boundary + 1, kill_committed) >= 1,
        "the committed gap must carry real lane work",
    );

    prover_a1.shutdown.open();
    let join_a1 = prover_a1.settler.await;
    prover_a1.node.shutdown();
    assert!(join_a1.is_ok(), "run 1 settler panicked: {join_a1:?}");
    // The kill leaves the scheduler's execution workers trailing the shutdown for seconds; let
    // them finish so the store lock is released before reopening (as in the sibling tests). The
    // test's own handle clones must drop too: they hold the same RocksDB lock.
    drop(journal);
    drop(store);
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The journal did not grow across the kill: every surviving entry was already recorded at
    // the kill decision (late straggler landings may have compacted some away, which only
    // widens the gap the restart must cover).
    {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("committed-gap journal after run 1: bundle {start}..={}", entry.end_index);
        }
        assert!(
            entries.iter().all(|(start, _)| *start <= kill_journal_max),
            "the kill must not journal the parked tail (journal max at kill {kill_journal_max})",
        );
    }
    let kill_len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();

    // === Run 2 over the SAME store: the startup gap pass must re-form the committed range,
    // settle it, and let new work chain past it. ===
    let kp_a2 = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a2 = prover_address(&kp_a2, network_id);
    l1.fund_address(&addr_a2, FUND_VALUE, FUND_COUNT).await;
    let prover_a2 = spawn_prover_on_store(
        &l1,
        "A2",
        kp_a2,
        addr_a2.clone(),
        4..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state,
        elfs,
        None,
        Some(block_deploy),
        open_store_retrying(db_dir.path()),
        false,
    )
    .await;

    let mut final_len = kill_len;
    for i in 0..20 {
        drive_range(&l1).await;
        final_len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!("committed-gap run2 driver: iteration {i}, covenant chain length {final_len}");
        if final_len > kill_len {
            break;
        }
    }
    // Compaction window for the re-formed gap entry and any in-flight tail, as in the siblings.
    for _ in 0..30 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    prover_a2.shutdown.open();
    let join_a2 = prover_a2.settler.await;
    prover_a2.node.shutdown();
    // No fatal settler stop across the re-formed gap bundle.
    assert!(join_a2.is_ok(), "run 2 settler stopped on the re-formed gap: {join_a2:?}");

    assert!(
        final_len > kill_len,
        "run 2 must settle the committed gap and new work past it ({kill_len} -> {final_len})",
    );
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    // Run 1's journal entries and the re-formed gap entry compact away against the advanced
    // tip; anything surviving starts strictly above the kill boundary.
    {
        let journal = StoreJournal::new(open_store_retrying(db_dir.path()));
        let entries = journal.entries();
        for (start, entry) in &entries {
            eprintln!("committed-gap journal after run 2: bundle {start}..={}", entry.end_index);
        }
        assert!(
            entries.iter().all(|(start, _)| *start > kill_boundary),
            "entries at or below the kill boundary {kill_boundary} must compact once the gap \
             settles",
        );
    }

    l1.shutdown().await;
}

/// A settlement the chain already holds must confirm through the settler's own chain probe when
/// the settlement watch never observes it. The production wedge (two tn10 incidents): the
/// settlement was mined and accepted within a second, but the bridge's settlement watch stayed
/// frozen below the accepting block, so the notification-based confirm wait never fired and the
/// single-flight settler waited forever with nothing settling after it.
///
/// The blindness is injected directly (a settlement channel nobody writes, standing in for the
/// frozen bridge): simnet cannot reproduce the multi-second chain fetch that froze the production
/// bridge, and the settler's behavior under a never-advancing watch is identical. Every
/// confirmation must then resolve on the confirm-wait warn tick's chain probe, the covenant must
/// stay a single chain, and subsequent bundles must keep settling.
#[tokio::test(flavor = "multi_thread")]
async fn settler_confirms_with_blind_settlement_watch() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping settler_confirms_with_blind_settlement_watch: RISC0_DEV_MODE!=1 - the \
             blind-watch scenario runs dev stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    // === Step 0: simnet L1 + dev covenant bootstrap (same shape as the warm-restart tests) ===
    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &Hash::default());
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
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
    let addr_a = prover_address(&kp_a, network_id);
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;
    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);

    // === The blind-watch prover: the settler's only confirmation path is the warn tick's chain
    // probe, exactly as in the incident. ===
    let db_dir = TempDir::new().expect("persistent store dir");
    let prover = spawn_prover_on_store(
        &l1,
        "A",
        kp_a,
        addr_a.clone(),
        1..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        bootstrap_state,
        elfs,
        None,
        Some(block_deploy),
        RunnerStore::open(db_dir.path()),
        true,
    )
    .await;

    // === Drive work and watch the covenant chain grow. With the watch blind, every confirmation
    // resolves on a warn tick, so the chain length is the observable: >= 2 proves the first
    // settlement confirmed through the chain probe (without the backstop the single-flight
    // settler parks forever at length 1) and a subsequent bundle settled on top. The deadline
    // bounds the wait to a handful of warn ticks. ===
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(180);
    let mut settled = 0usize;
    let mut iteration = 0usize;
    while std::time::Instant::now() < deadline {
        drive_range(&l1).await;
        settled = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        eprintln!(
            "blind-watch driver: iteration {iteration}, covenant chain length {settled} ({}s \
             elapsed)",
            started.elapsed().as_secs(),
        );
        if settled >= 2 {
            break;
        }
        iteration += 1;
    }
    assert!(
        settled >= 2,
        "with a blind settlement watch the settler must confirm via the chain probe and keep \
         settling; chain length {settled} after {}s (without the backstop the first confirm \
         wedges the single-flight settler forever)",
        started.elapsed().as_secs(),
    );

    prover.shutdown.open();
    let join = prover.settler.await;
    prover.node.shutdown();
    assert!(join.is_ok(), "the settler stopped under the blind watch: {join:?}");
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    l1.shutdown().await;
}

/// A single prover joins an ALREADY-LIVE lane, modeling a re-deploy over a lane warmed by
/// previous runs: the lane is warmed with carrier txs BEFORE the covenant is deployed, the
/// bootstrap pins the lane's authoritative tip (resolved via `get_seq_commit_lane_proof` at the
/// sink) into its redeem, and only then does the prover (and its bridge) exist. The bridge
/// anchors its fresh sink at the current sink block (the `seed_depth: 0`, no `start_from`
/// path), where the lane is live.
///
/// This guards the bridge's authoritative lane-tip seeding: without it the genesis anchor
/// carries the zero lane seed, the first post-join carrier anchors at the parent seq commit,
/// and every derived tip diverges from the lane tip consensus chains from - the first
/// artifact's prev tip mismatches the bootstrap's pinned tip, so the settler's redeem-prefix
/// assert panics (or the node rejects the settlement with a seq-commit script failure) and the
/// covenant chain never grows past the bootstrap. With the seeding, the prover proves and
/// settles post-join ranges onto the single contiguous chain.
#[tokio::test(flavor = "multi_thread")]
async fn prover_joins_live_lane() {
    if !dev_mode_enabled() {
        eprintln!(
            "skipping prover_joins_live_lane: RISC0_DEV_MODE!=1 - the live-lane join runs dev \
             stub proofs + the dev redeem on CPU",
        );
        return;
    }
    let _serial = serialize_settlement_test().await;

    // === Step 0: simnet L1 (same config as two_provers_contend) ===
    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: warm the lane BEFORE anything else exists ===
    // Mine lane carriers with nobody watching, one per block, so the seq-commit SMT folds live
    // tips no bridge ever observed. This models a re-deploy over a lane warmed by previous runs.
    for i in 0..4 {
        let payload =
            encode_activity_payload(&[AccessMetadata::write(ResourceId::for_test(1))], &[1, 2, 3]);
        let carrier = l1
            .build_subnet_payload_transactions(vec![payload], LANE_SUBNET, TX_VERSION_TOCCATA)
            .await
            .into_iter()
            .next()
            .expect("carrier tx");
        l1.mine_block(std::slice::from_ref(&carrier)).await;
        eprintln!("live-lane warmup: mined carrier {i}");
    }
    l1.mine_blocks(2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // === Step 2: resolve the lane's authoritative tip, then bootstrap onto it ===
    // A covenant deployed onto an already-live lane must pin that lane's live tip into its
    // redeem: the first settlement chains from the tip the bridge seeds its genesis with, so a
    // zero pin would mismatch at the settler's redeem-prefix assert. No lane activity lands
    // between this lookup and the prover's spawn, so the pinned tip and the bridge's seeded
    // genesis tip agree.
    let client = connect_wrpc(&l1.wrpc_borsh_url(), network_id).await;
    let sink = client.get_block_dag_info().await.expect("dag info").sink;
    let live_tip = client
        .get_seq_commit_lane_proof(sink, lane_key)
        .await
        .expect("lane proof at the warmed lane")
        .lane
        .expect("the warmed lane holds a live entry at the sink")
        .tip;
    eprintln!("live-lane join: authoritative tip at sink {sink} is {live_tip}");

    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key, &live_tip);
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
        lane_tip: live_tip,
        outpoint: bootstrap_outpoint,
        spk: bootstrap_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };

    // === Step 3: fund ONE prover, then spawn it onto the live lane ===
    let kp = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr = prover_address(&kp, network_id);
    l1.fund_address(&addr, FUND_VALUE, FUND_COUNT).await;
    eprintln!("funded live-lane prover address {addr}");

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);

    // `start_from: None` puts the bridge on the `seed_depth: 0` path (anchor at the sink, where
    // the lane is live) and the settler on the fresh-deploy path (nobody has settled yet, so the
    // bootstrap outpoint IS unspent). No alternation partner, so the solo prover settles every
    // range it forms: deterministic, no spend race.
    let prover = spawn_prover(
        &l1,
        "P",
        kp,
        addr.clone(),
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

    // === Step 4: drive post-join ranges so the prover proves and settles them ===
    for i in 0..4 {
        eprintln!("live-lane driver: iteration {i}");
        drive_range(&l1).await;
    }

    // === Step 5: drain in-flight settlements, then tear down ===
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
        if stable_rounds >= 3 && len >= 2 {
            break;
        }
        if round % 5 == 0 {
            eprintln!("live-lane drain: round {round}, covenant chain length {len}");
        }
    }

    prover.shutdown.open();
    let join = prover.settler.await;
    prover.node.shutdown();

    // === Assertions ===
    assert!(join.is_ok(), "live-lane prover settler panicked: {join:?}");

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

    eprintln!("live-lane join: final covenant chain length = {}", chain.len());
    assert!(
        chain.len() >= 2,
        "expected a covenant chain of at least 2 settlements after joining the live lane, got {}",
        chain.len(),
    );

    // No settlement landed on a DAG side-branch.
    assert_no_fork(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;

    l1.shutdown().await;
}

/// Lane carriers mined per settlement range in the catch-up test (the bundle minimum).
const CATCHUP_CARRIERS: usize = 2;

/// Mines `count` lane carrier blocks (one carrier tx per block) and NOTHING else, so the provers'
/// bridges batch them but any settlement they trigger stays unconfirmed in the mempool: a
/// settlement confirms only when a LATER block mines, so withholding acceptance blocks is the
/// deterministic pending-tail lever.
async fn mine_lane_carriers(l1: &L1Node, count: usize) {
    for i in 0..count {
        let payload =
            encode_activity_payload(&[AccessMetadata::write(ResourceId::for_test(1))], &[1, 2, 3]);
        let carrier = l1
            .build_subnet_payload_transactions(vec![payload], LANE_SUBNET, TX_VERSION_TOCCATA)
            .await
            .into_iter()
            .next()
            .expect("carrier tx");
        l1.mine_block(std::slice::from_ref(&carrier)).await;
        eprintln!("carrier driver: mined carrier {i}");
    }
}

/// Mines one settlement range for the catch-up test: a bundle's worth of lane carriers, then
/// several acceptance blocks (with pauses) so pending settlements land and confirm on chain.
async fn drive_range(l1: &L1Node) {
    mine_lane_carriers(l1, CATCHUP_CARRIERS).await;
    for _ in 0..5 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Polls the node's mempool until a transaction spending `outpoint` appears (bounded, with pacing
/// logs), then returns. The settlement worker submits each bundle's settlement as soon as its
/// artifact is published, and the aggregate prover journals the bundle in that same step, so a
/// mempool entry spending the covenant tip proves the next bundle was proved, journaled, and
/// submitted. The test holds mining while this waits, keeping the polled settlement
/// deterministically unconfirmed. The bound is generous (proving plus submission takes seconds
/// under CI load); a hit ends the wait early. Panics on timeout so a stalled pipeline fails
/// loudly at the point it stalled rather than in a later chain assertion.
async fn await_mempool_spender(l1: &L1Node, outpoint: TransactionOutpoint) {
    for round in 0..240 {
        let entries =
            l1.grpc_client().get_mempool_entries(false, false).await.expect("get_mempool_entries");
        let found = entries.iter().any(|entry| {
            let Ok(tx) = Transaction::try_from(entry.transaction.clone()) else {
                return false;
            };
            tx.inputs.first().is_some_and(|input| input.previous_outpoint == outpoint)
        });
        if found {
            eprintln!("mempool: settlement spending {outpoint} submitted (round {round})");
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("no mempool transaction spending {outpoint} appeared within the bounded wait");
}

/// Returns whether the node's mempool holds no settlement of `covenant_id`.
async fn mempool_settlement_free(l1: &L1Node, covenant_id: Hash) -> bool {
    let entries =
        l1.grpc_client().get_mempool_entries(false, false).await.expect("get_mempool_entries");
    !entries.iter().any(|entry| {
        let Ok(tx) = Transaction::try_from(entry.transaction.clone()) else {
            return false;
        };
        tx.settlement_info(covenant_id, Hash::default(), 0).is_some()
    })
}

/// Polls until the node's mempool holds no settlement of `covenant_id`, mining one block per
/// round so a submitted-but-unconfirmed settlement lands and clears. With the mempool clean, the
/// chain tip is the last landed settlement, and the next settlement to appear can only belong to
/// work proved after this point: the pending-tail construction (carriers mined with no
/// acceptance blocks) then journals exactly one unsettled bundle whose submission is the sole
/// mempool occupant at the kill. Panics on timeout.
async fn await_mempool_settlement_free(l1: &L1Node, covenant_id: Hash) {
    for round in 0..80 {
        if mempool_settlement_free(l1, covenant_id).await {
            eprintln!("mempool: settlement-free (round {round})");
            return;
        }
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the mempool still holds a settlement after the bounded wait");
}

/// Polls until the node's mempool holds a settlement of `covenant_id` proving through one of
/// `blocks`, mining one block every `mine_every` rounds so a joining prover's bridge keeps
/// paging its replay forward (the initial chain fetch is server-capped, and a static chain
/// produces no notifications to page it). The poll runs BEFORE each mined block, so a
/// submission observed here is still unconfirmed. Panics on timeout.
async fn await_mempool_proving_through(l1: &L1Node, covenant_id: Hash, blocks: &[Hash]) {
    for round in 0..160 {
        let entries =
            l1.grpc_client().get_mempool_entries(false, false).await.expect("get_mempool_entries");
        let found = entries.iter().any(|entry| {
            let Ok(tx) = Transaction::try_from(entry.transaction.clone()) else {
                return false;
            };
            tx.settlement_info(covenant_id, Hash::default(), 0)
                .is_some_and(|info| blocks.contains(&info.block_prove_to))
        });
        if found {
            eprintln!("mempool: settlement proving through an interior block (round {round})");
            return;
        }
        if round % 6 == 5 {
            l1.mine_blocks(1).await;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("no settlement proving through the given blocks appeared within the bounded wait");
}

/// Opens the store at `dir`, retrying while the just-killed prover's trailing work still holds
/// the RocksDB lock (an in-flight lane-proof fetch parked in its RPC timeout, or a trailing
/// batch execution, keeps the store Arc alive past the node's shutdown; the fetch's timeout is
/// ten seconds, so the bound covers it). Panics once the bounded wait passes.
fn open_store_retrying(dir: &std::path::Path) -> RunnerStore {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut last_err = None;
    for attempt in 0..400 {
        let opened =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| RunnerStore::open(dir)));
        match opened {
            Ok(store) => {
                std::panic::set_hook(prev_hook);
                if attempt > 0 {
                    eprintln!("store: lock released after {attempt} retries");
                }
                return store;
            }
            Err(err) => {
                last_err = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    std::panic::set_hook(prev_hook);
    let err = last_err.expect("at least one failed attempt");
    // `resume_unwind` re-raises the original panic without invoking the (restored) hook, and the
    // original panic ran under the silent hook above, so without this line a real lock timeout
    // fails the test with no message at all.
    eprintln!(
        "store: opening {} still failing after the bounded retries (lock held or unreadable): \
         {:?}",
        dir.display(),
        err.downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| err.downcast_ref::<&str>().copied()),
    );
    std::panic::resume_unwind(err);
}

/// Returns whether checkpoint `index` holds a durable per-batch receipt in the proof-receipt
/// column family. A per-batch receipt key is `checkpoint_index || block_hash || image_id` (72
/// bytes), distinguished by length from the 76-byte per-tx and 104-byte aggregate keys that
/// share the checkpoint prefix.
fn batch_receipt_present(store: &RunnerStore, index: u64) -> bool {
    store
        .prefix_iter(StateSpace::ProofReceipt, &index.to_be_bytes())
        .any(|(key, _)| key.len() == 72)
}

/// Returns whether every batch in `lower..=upper` has its metadata and, when it carries lane
/// activity, a durable per-batch receipt: the exact inputs the startup gap re-formation reads.
/// An empty batch (its block carried no lane tx, so the lane tip carried forward) proves no
/// receipt and is skipped, matching the live bundle filter.
fn gap_batches_ready(
    journal: &StoreJournal<RunnerStore>,
    store: &RunnerStore,
    lower: u64,
    upper: u64,
) -> bool {
    (lower..=upper).all(|index| {
        let Some(metadata) = journal.batch_metadata(index) else { return false };
        metadata.lane_tip == metadata.prev_lane_tip || batch_receipt_present(store, index)
    })
}

/// Counts batches in `lower..=upper` that carry lane activity: the ones whose work the gap
/// re-formation must recover for the restarted covenant to advance.
fn gap_real_batches(journal: &StoreJournal<RunnerStore>, lower: u64, upper: u64) -> usize {
    (lower..=upper)
        .filter(|&index| {
            journal
                .batch_metadata(index)
                .is_some_and(|metadata| metadata.lane_tip != metadata.prev_lane_tip)
        })
        .count()
}

/// One link in the covenant continuation chain: a settlement tx, the covenant outpoint its input 0
/// spends, the SPKs of all its outputs (so attribution can match a prover's change SPK), and the
/// L1 block the settlement proves through (the boundary a competitor can straddle).
struct CovenantLink {
    /// The settlement transaction's id.
    tx_id: Hash,
    /// Covenant outpoint this settlement's input 0 spends.
    covenant_input: TransactionOutpoint,
    /// SPKs of all outputs, used to attribute the settlement to a prover by its change SPK.
    change_spks: Vec<kaspa_consensus_core::tx::ScriptPublicKey>,
    /// L1 block hash this settlement proves up to, decoded from its settlement tail.
    block_prove_to: Hash,
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
    let db_dir = TempDir::new().expect("temp dir");
    let mut prover = spawn_prover_over_store(
        l1,
        label,
        keypair,
        address,
        bundle_size,
        network_id,
        params,
        lane_key,
        covenant_id,
        covenant,
        elfs,
        alternation,
        start_from,
        db_dir.path(),
    )
    .await;
    prover._db_dir = Some(db_dir);
    prover
}

/// Warm-store variant of [`spawn_prover`]: the node opens the db at `db_dir`, so a later run over
/// the same dir resumes its journal, receipts, and scheduler frontier. The caller owns the dir's
/// lifetime (the returned [`Prover`] holds no guard over it), so the store must be released by
/// `node.shutdown()` before the dir is dropped or reopened.
#[allow(clippy::too_many_arguments)]
async fn spawn_prover_over_store(
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
    db_dir: &std::path::Path,
) -> Prover {
    spawn_prover_on_store(
        l1,
        label,
        keypair,
        address,
        bundle_size,
        network_id,
        params,
        lane_key,
        covenant_id,
        covenant,
        elfs,
        alternation,
        start_from,
        open_store_retrying(db_dir),
        false,
    )
    .await
}

/// Same as [`spawn_prover_over_store`] over a caller-opened store handle: the caller keeps a clone
/// of the handle for direct column-family reads while the node runs (the handle is shared, not
/// reopened, so the RocksDB lock is never contended). `blind_settlement_watch` hands the prover
/// and its settler a settlement channel nobody writes: the blind-watch incident shape, where the
/// settler's only confirmation path is its own chain probe.
#[allow(clippy::too_many_arguments)]
async fn spawn_prover_on_store(
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
    store: RunnerStore,
    blind_settlement_watch: bool,
) -> Prover {
    let wrpc_url = l1.wrpc_borsh_url();
    // Separate wRPC clients for the lane source and the settler so they own independent handles.
    let client_for_lane = connect_wrpc(&wrpc_url, network_id).await;
    let client_for_settler = connect_wrpc(&wrpc_url, network_id).await;

    let queue = SettlementQueue::new();
    // Each prover follows the same L1 through its own bridge, so each gets its own live settlement
    // channel: the bridge (writer) publishes settlements it observes (including the competitor's),
    // and this prover's settler (reader) reconciles against them.
    let (settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    // The blind watch reproduces the production incident shape: the never-written channel stands
    // in for a bridge whose settlement publication froze below the accepting block, leaving the
    // settler's own chain probe as the only confirmation path. Its sender is leaked on purpose:
    // dropping it would read as bridge teardown, which the settler treats as shutdown.
    let watch_rx = if blind_settlement_watch {
        let (blind_tx, blind_rx) = watch::channel(None::<SettlementInfo>);
        std::mem::forget(blind_tx);
        blind_rx
    } else {
        settlement_rx
    };
    // The prover's own journal over its own store, matching the pre-threading wiring; the settler
    // below stays journal-free so this test's skip behavior is unchanged.
    let journal: Arc<dyn SettlementJournal> = Arc::new(StoreJournal::new(store.clone()));
    let node = build_proving_node(
        elfs,
        store,
        BridgeParams {
            url: wrpc_url,
            network_id,
            lane_subnet: LANE_SUBNET,
            covenant_id,
            finality_depth: params.finality_depth(),
            seed_depth: 0,
            min_confirmations: None,
            adaptive_filter_disabled: false,
            start_from,
            observers: BridgeObservers {
                tip_daa: None,
                settlement: Some(settlement_tx),
                settlement_events: None,
                permission_spends: None,
            },
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
            settlement_rx: Some(watch_rx.clone()),
            journal,
            exits_tx: None,
        },
        None,
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
            settlement: watch_rx,
            // Jitter each submission so neither prover deterministically wins the spend race for
            // every range; without it the first-spawned prover lands every settlement. The window
            // is wide relative to the dev proving time so the per-range winner is a
            // genuine coin flip.
            submit_jitter: Some(0..40),
            journal: None,
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
    Prover { node, settler, shutdown, _db_dir: None }
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
            let Some(info) = tx.settlement_info(covenant_id, cursor, 0) else {
                continue;
            };
            let covenant_input = tx.inputs.first().expect("settlement input").previous_outpoint;
            let change_spks =
                tx.outputs.iter().map(|o| o.script_public_key.clone()).collect::<Vec<_>>();
            by_input.insert(
                covenant_input,
                CovenantLink {
                    tx_id: tx.id(),
                    covenant_input,
                    change_spks,
                    block_prove_to: info.block_prove_to,
                },
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
