//! Seeded L2-flow simulation: a node issues lane-activity transactions, the driver follows the
//! chain and executes them through the zk `Vm`, asserting every block that the decoded counter
//! equals the number of lane transactions executed. A fixed seed makes any failure reproducible.

use rand::{RngCore, SeedableRng, rngs::StdRng};
use secp256k1::{Keypair, Secp256k1};
use simpa::simulator::miner::{Miner, MinerOptions, NativeLaneProducer};
use vprogs_sim::{
    config::sim_config,
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
}

/// Drives a seeded simulation: miner 0 runs the L2 driver, miners 1.. are plain filler miners (DAG
/// width + reorgs). Returns the driver's running totals.
fn run_sim(p: SimParams) -> DriverStats {
    let SimParams { seed, num_miners, target_blocks, bps, delay, enable_settlements } = p;
    let config = sim_config(bps, delay);
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
            let (driver, handle) = L2Driver::new(L2Config {
                lane_id,
                seed: seed ^ 0xA5A5,
                activity_per_block: 3,
                enable_settlements,
                settle_every: 15,
            });
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
    });
    println!("settlements: {s:?}");
    assert!(s.activity_executed > 0, "expected some lane activity to be executed");
    assert!(s.settlements_accepted > 0, "expected covenant settlements to land and chain");
}
