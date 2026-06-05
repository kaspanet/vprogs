//! Smoke test: the vprogs-built simulated network (reusing simpa's filler miners) produces a chain.
//! Validates the consensus + sim-engine integration before the L2 driver is layered on.

use rand::{RngCore, SeedableRng, rngs::StdRng};
use secp256k1::Secp256k1;
use simpa::simulator::miner::{Miner, MinerOptions, NativeLaneProducer};
use vprogs_sim::{config::sim_config, network::SimNetwork};

#[test]
fn sim_network_produces_blocks() {
    kaspa_core::log::try_init_logger("warn");

    let seed = 1u64;
    let bps = 2.0;
    let delay = 1.0;
    let num_miners = 2u64;
    let target_blocks = 300u64;

    let config = sim_config(bps, delay);
    let mut net = SimNetwork::new((delay * 1000.0) as u64, config.genesis.timestamp);

    let secp = Secp256k1::new();
    let mut rng = StdRng::seed_from_u64(seed);
    for i in 0..num_miners {
        // The clone goes straight into the miner (which the engine drops when the run ends); the
        // network keeps no long-lived clone of its own.
        let consensus = net.add_node(&config);
        let (sk, pk) = secp.generate_keypair(&mut rng);
        let miner_rng = StdRng::seed_from_u64(rng.next_u64());
        let miner = Miner::new(
            i,
            bps,
            1.0 / num_miners as f64,
            sk,
            pk,
            consensus,
            &config.params,
            MinerOptions {
                rng: miner_rng,
                target_txs_per_block: 50,
                target_blocks: Some(target_blocks),
                long_payload: false,
                lane_producer: Box::new(NativeLaneProducer),
            },
        );
        net.register(i, Box::new(miner));
    }

    net.run(u64::MAX);

    let sink_blue = net.sink_blue_score(0);
    assert!(sink_blue > 0, "expected blocks to be produced, sink blue score = {sink_blue}");
    net.shutdown();
}
