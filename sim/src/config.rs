//! Consensus configuration for the L2 simulation.
//!
//! Modeled on simpa's `--test-pruning` profile (the same one `smt_repro` uses), with the covenant
//! forks forced active so settlement scripts validate. Finality and pruning are kept small so a run
//! reaches a steady state (and starts pruning) within a few thousand blocks.

use std::sync::Arc;

use kaspa_consensus::{
    config::{Config, ConfigBuilder},
    constants::perf::PerfParams,
    model::stores::ghostdag::KType,
    params::{DEVNET_PARAMS, ForkActivation, NETWORK_DELAY_BOUND, Params},
};
use kaspa_consensus_core::config::bps::calculate_ghostdag_k;
use vprogs_zk_backend_risc0_test_suite::force_covenant_forks;

/// Finality depth used by the sim (also drives lane expiry).
pub const FINALITY_DEPTH: u64 = 200;
/// Pruning depth; a few finality windows so pruning actually runs over a long simulation.
pub const PRUNING_DEPTH: u64 = 200 * 2 + 50;

/// Builds the simulation consensus config for `bps`/`delay`, with Toccata + zk-hardening (and
/// crescendo) forced active.
pub fn sim_config(bps: f64, delay: f64) -> Arc<Config> {
    sim_config_with_maturity(bps, delay, None)
}

/// Like [`sim_config`] but pins `coinbase_maturity` to `coinbase_maturity` blocks when `Some`. A
/// low value lets matured coinbase (hence lane activity) appear within a few dozen blocks, which
/// keeps the real-proof (cuda) run short; `None` uses the default delay-scaled maturity.
pub fn sim_config_with_maturity(
    bps: f64,
    delay: f64,
    coinbase_maturity: Option<u64>,
) -> Arc<Config> {
    let mut params = DEVNET_PARAMS;
    apply_sim_params(&mut params, bps, delay, coinbase_maturity);
    force_covenant_forks(&mut params);
    Arc::new(
        ConfigBuilder::new(params)
            .apply_args(|config| apply_perf_params(&mut config.perf))
            .adjust_perf_params_to_consensus_params()
            .skip_proof_of_work()
            .enable_sanity_checks()
            .build(),
    )
}

/// Tunes consensus params for a fast, small-finality simulation (mirrors `smt_repro`'s
/// `apply_pruning_repro_params`, minus the toccata toggle which the caller forces).
fn apply_sim_params(params: &mut Params, bps: f64, delay: f64, coinbase_maturity: Option<u64>) {
    params.coinbase_maturity = 200;

    let max_delay = delay.max(NETWORK_DELAY_BOUND as f64);
    let k = u64::max(calculate_ghostdag_k(2.0 * max_delay * bps, 0.05), params.ghostdag_k() as u64);
    let k = u64::min(k, KType::MAX as u64) as KType;
    params.ghostdag_k = k;
    params.target_time_per_block = (1000.0 / bps) as u64;
    params.crescendo_activation = ForkActivation::always();
    params.timestamp_deviation_tolerance = 16;
    params.past_median_time_sample_rate = (10.0 * bps) as u64;
    params.difficulty_sample_rate = (2.0 * bps) as u64;

    params.pruning_proof_m = 16;
    params.min_difficulty_window_size = 16;
    params.difficulty_window_size = params.difficulty_window_size.min(32);

    params.ghostdag_k = 20;
    params.finality_depth = FINALITY_DEPTH;
    params.merge_depth = 64 * 2;
    params.mergeset_size_limit = 32 * 2;
    params.max_block_parents = u8::max((0.66 * params.ghostdag_k as f64) as u8, 10);
    params.pruning_depth = PRUNING_DEPTH;
    params.coinbase_maturity = coinbase_maturity.unwrap_or_else(|| {
        (params.coinbase_maturity as f64 * f64::max(1.0, bps * delay * 0.25)) as u64
    });

    params.storage_mass_parameter = 10_000;
}

/// Two worker threads each for block and virtual processing, matching `smt_repro`.
fn apply_perf_params(perf: &mut PerfParams) {
    perf.block_processors_num_threads = 2;
    perf.virtual_processor_num_threads = 2;
}
