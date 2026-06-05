//! Seeded L2-flow simulation: a node issues lane-activity transactions, the driver follows the
//! chain and executes them through the zk `Vm`, asserting every block that the decoded counter
//! equals the number of lane transactions executed. A fixed seed makes any failure reproducible.

use rand::{RngCore, SeedableRng, rngs::StdRng};
use secp256k1::{Keypair, Secp256k1};
use simpa::simulator::miner::{Miner, MinerOptions, NativeLaneProducer};
use vprogs_sim::{
    config::sim_config_with_maturity,
    driver::{DriverStats, L2Config, L2Driver},
    l2_miner::L2Miner,
    network::SimNetwork,
};

/// Parameters for a simulation run.
struct SimParams {
    seed: u64,
    num_miners: u64,
    target_blocks: u64,
    bps: f64,
    delay: f64,
    enable_settlements: bool,
    /// Drive the real batch prover off the sim consensus (GPU proofs under `--features cuda`; the
    /// full proving path still runs under `RISC0_DEV_MODE=1` with stub receipts).
    enable_proving: bool,
    /// Override `coinbase_maturity` (blocks). `None` uses the default delay-scaled maturity
    /// (~200); a small value makes matured coinbase — hence lane activity — appear early,
    /// keeping a real-proof run short.
    coinbase_maturity: Option<u64>,
    /// Batches bundled per proof when `enable_proving` is set (also the real-proof settlement
    /// cadence: one settlement per completed bundle).
    bundle_size: usize,
}

/// Drives a seeded simulation: miner 0 runs the L2 driver, miners 1.. are plain filler miners (DAG
/// width + reorgs). Returns the driver's running totals.
fn run_sim(p: SimParams) -> DriverStats {
    let SimParams {
        seed,
        num_miners,
        target_blocks,
        bps,
        delay,
        enable_settlements,
        enable_proving,
        coinbase_maturity,
        bundle_size,
    } = p;
    let config = sim_config_with_maturity(bps, delay, coinbase_maturity);
    let mut net = SimNetwork::new((delay * 1000.0) as u64, config.genesis.timestamp);

    let secp = Secp256k1::new();
    let mut rng = StdRng::seed_from_u64(seed);
    let lane_id = 4444u32;
    let mut stats = None;

    for i in 0..num_miners {
        let consensus = net.add_node(&config);
        let (sk, pk) = secp.generate_keypair(&mut rng);
        let miner_rng = StdRng::seed_from_u64(rng.next_u64());
        let hashrate = 1.0 / num_miners as f64;

        if i == 0 {
            let (driver, handle) = L2Driver::new(
                L2Config {
                    lane_id,
                    seed: seed ^ 0xA5A5,
                    activity_per_block: 3,
                    enable_settlements,
                    settle_every: 15,
                    enable_proving,
                    bundle_size,
                },
                &consensus,
            );
            stats = Some(handle);
            let miner = L2Miner::new(
                i,
                bps,
                hashrate,
                Keypair::from_secret_key(secp256k1::SECP256K1, &sk),
                consensus,
                &config.params,
                miner_rng,
                Some(target_blocks),
                16,
                Box::new(driver),
            );
            net.register(i, Box::new(miner));
        } else {
            let miner = Miner::new(
                i,
                bps,
                hashrate,
                sk,
                pk,
                consensus,
                &config.params,
                MinerOptions {
                    rng: miner_rng,
                    target_txs_per_block: 20,
                    target_blocks: Some(target_blocks),
                    long_payload: false,
                    lane_producer: Box::new(NativeLaneProducer),
                },
            );
            net.register(i, Box::new(miner));
        }
    }

    net.run(u64::MAX);

    let result = stats.unwrap().lock().unwrap().clone();
    net.shutdown();
    result
}

#[test]
fn l2_flow_single_miner_seed_1() {
    kaspa_core::log::try_init_logger("warn");
    let s = run_sim(SimParams {
        seed: 1,
        num_miners: 1,
        target_blocks: 300,
        bps: 1.0,
        delay: 0.1,
        enable_settlements: false,
        enable_proving: false,
        coinbase_maturity: None,
        bundle_size: 1,
    });
    println!("single-miner: {s:?}");
    assert!(s.blocks_processed > 0, "expected the driver to process blocks");
    assert!(s.activity_executed > 0, "expected some lane activity to be executed");
}

#[test]
fn l2_flow_is_deterministic() {
    kaspa_core::log::try_init_logger("warn");
    let p = || SimParams {
        seed: 7,
        num_miners: 1,
        target_blocks: 200,
        bps: 1.0,
        delay: 0.1,
        enable_settlements: true,
        enable_proving: false,
        coinbase_maturity: None,
        bundle_size: 1,
    };
    let a = run_sim(p());
    let b = run_sim(p());
    assert_eq!(a, b, "same seed must reproduce identical results: {a:?} vs {b:?}");
}

#[test]
fn l2_flow_two_miners_reorgs_seed_3() {
    kaspa_core::log::try_init_logger("warn");
    // Two miners with a non-trivial broadcast delay produce a real DAG whose selected chain reorgs;
    // the driver must roll the scheduler back and keep the counter invariant holding through it.
    let s = run_sim(SimParams {
        seed: 3,
        num_miners: 2,
        target_blocks: 300,
        bps: 2.0,
        delay: 1.0,
        enable_settlements: false,
        enable_proving: false,
        coinbase_maturity: None,
        bundle_size: 1,
    });
    println!("two-miner: {s:?}");
    assert!(s.blocks_processed > 0, "expected the driver to process blocks");
    assert!(s.activity_executed > 0, "expected some lane activity to be executed");
    assert!(s.reorgs > 0, "two miners with delay should produce reorgs");
}

#[test]
fn l2_flow_settlements_seed_2() {
    kaspa_core::log::try_init_logger("warn");
    // Single miner (clean chain) bootstraps a covenant and settles it repeatedly; each settlement
    // is validated by the sim's real script engine, so acceptance proves the anchor + state
    // chaining.
    let s = run_sim(SimParams {
        seed: 2,
        num_miners: 1,
        target_blocks: 400,
        bps: 1.0,
        delay: 0.1,
        enable_settlements: true,
        enable_proving: false,
        coinbase_maturity: None,
        bundle_size: 1,
    });
    println!("settlements: {s:?}");
    assert!(s.activity_executed > 0, "expected some lane activity to be executed");
    assert!(s.settlements_accepted > 0, "expected covenant settlements to land and chain");
    // A clean single-miner chain never orphans a tx, so no settlement is ever re-issued, and every
    // issued settlement lands.
    assert_eq!(s.reissues, 0, "single miner: no reorgs, so no re-issues expected");
    assert_eq!(
        s.settlements_issued, s.settlements_accepted,
        "single miner: every issued settlement must land",
    );
}

#[test]
fn l2_flow_settlements_reorgs_seed_5() {
    kaspa_core::log::try_init_logger("warn");
    // Settlements under a real reorging DAG: the covenant tx whose block is orphaned must be rolled
    // back off the confirmed history (or abandoned while pending) and re-issued from the live tip,
    // so the covenant keeps chaining and still makes progress. The run completing without the
    // script engine rejecting a settlement (which would panic block insertion) is the proof the
    // reorg handling kept the covenant consistent with the selected chain.
    let s = run_sim(SimParams {
        seed: 5,
        num_miners: 2,
        target_blocks: 400,
        bps: 2.0,
        delay: 1.0,
        enable_settlements: true,
        enable_proving: false,
        coinbase_maturity: None,
        bundle_size: 1,
    });
    println!("settlements+reorgs: {s:?}");
    assert!(s.reorgs > 0, "two miners with delay should produce reorgs");
    assert!(
        s.settlements_accepted > 0,
        "covenant must still settle and chain through reorgs (got {s:?})",
    );
    // Acceptance can only advance one confirmed state at a time, so issued >= accepted always; the
    // liveness guard inside the driver (MAX_REISSUES_WITHOUT_PROGRESS) panics if it ever wedges.
    assert!(s.settlements_issued >= s.settlements_accepted);
}

#[test]
fn l2_flow_proving_seed_1() {
    kaspa_core::log::try_init_logger("warn");
    // Drives the real batch prover (`ProvingPipeline::batch`) off the in-process consensus: the
    // scheduler submits each block's batch to the prover worker, which fetches that block's lane
    // proof through `ConsensusLaneSource` and proves a bundle. Under `RISC0_DEV_MODE=1` the proofs
    // are dev stubs (no GPU) but the entire wiring runs — `ConsensusLaneSource`, the worker's
    // derived-vs-consensus `lane_tip` sanity check, and the `Weak<Consensus>` teardown discipline
    // (no `DbLifetime` "DB has N strong references" panic on shutdown). On the GPU box the same
    // test built `--features cuda` and run *without* `RISC0_DEV_MODE` produces real proofs.
    //
    // Single miner so the chain is clean: a reorg would orphan a block whose batch the async worker
    // might already be proving, and the lane-proof fetch for a no-longer-selected block would fail.
    // Low coinbase maturity so spendable coinbase — hence lane activity — appears within a few
    // dozen blocks, keeping the real-proof run short (each bundle is a full proof on the GPU).
    let s = run_sim(SimParams {
        seed: 1,
        num_miners: 1,
        target_blocks: 80,
        bps: 1.0,
        delay: 0.1,
        enable_settlements: false,
        enable_proving: true,
        coinbase_maturity: Some(20),
        bundle_size: 1,
    });
    println!("proving: {s:?}");
    assert!(s.blocks_processed > 0, "expected the driver to process blocks");
    // Real lane txs are scheduled and bundled through the prover (not just empty blocks): the
    // worker fetched each block's lane proof, the derived-vs-consensus lane_tip check passed,
    // and the run tore down without a DbLifetime panic.
    assert!(s.activity_executed > 0, "expected lane activity to be executed and proved");
}

#[test]
fn l2_flow_real_proof_settlement_chain() {
    use vprogs_zk_backend_risc0_test_suite::dev_mode_enabled;

    // The end-to-end target: real proofs *and* real on-chain settlements. The driver bootstraps a
    // production covenant, the batch prover proves each bundle into a real receipt, and the driver
    // builds a production `Settlement::build` whose `OpZkPrecompile` the sim's script engine
    // validates against the real receipt before the settlement lands. The covenant advances
    // bootstrap → s1 → s2 → …, each settlement spending the previous continuation UTXO and chaining
    // `prev_state`/`prev_lane_tip` from the bundle journal.
    //
    // Real settlements require real receipts: `OpZkPrecompile` rejects dev stubs, so this test only
    // matters built `--features cuda --release` and run *without* `RISC0_DEV_MODE` on the GPU box.
    // It is skipped under dev mode (where the dev-settlement tests above cover the chain-side
    // chaining invariants without the precompile).
    if dev_mode_enabled() {
        eprintln!(
            "skipping l2_flow_real_proof_settlement_chain: RISC0_DEV_MODE=1 (needs real receipts)"
        );
        return;
    }
    kaspa_core::log::try_init_logger("warn");

    // Single miner so the chain is clean: the async prover never has a bundle's block orphaned out
    // from under it, and every issued settlement lands (issued == accepted, no re-issues). Low
    // coinbase maturity so activity — hence bundles — start within a few dozen blocks.
    // `bundle_size` throttles GPU cost (one proof per `bundle_size` blocks) and the settlement
    // cadence.
    let s = run_sim(SimParams {
        seed: 11,
        num_miners: 1,
        target_blocks: 160,
        bps: 1.0,
        delay: 0.1,
        enable_settlements: true,
        enable_proving: true,
        coinbase_maturity: Some(20),
        bundle_size: 10,
    });
    println!("real-proof e2e: {s:?}");
    // Bootstrap → at least three chained real settlements, each validated on chain by the
    // precompile against a real receipt and spending the prior continuation UTXO.
    assert!(
        s.settlements_accepted >= 3,
        "must chain bootstrap → s1 → s2 → s3 with real proofs (got {})",
        s.settlements_accepted,
    );
    // Clean single miner: no orphans, so no re-issues and every issued settlement lands.
    assert_eq!(s.reissues, 0, "single miner: no reorgs, so no re-issues expected");
    assert_eq!(
        s.settlements_issued, s.settlements_accepted,
        "single miner: every issued settlement must land",
    );
    assert!(s.activity_executed > 0, "expected lane activity to be executed and proved");
}
