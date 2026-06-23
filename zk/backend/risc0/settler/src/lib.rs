//! Settlement worker: lands proven bundles on L1.
//!
//! The [`AggregateProver`](vprogs_zk_aggregate_prover::AggregateProver) proves each bundle and
//! hands off a [`SettlementArtifact`](vprogs_zk_aggregate_prover::SettlementArtifact) (the
//! aggregate receipt plus its decoded state-transition bounds). This worker owns the on-chain side:
//! it builds a [`Settlement`](vprogs_zk_backend_risc0_covenant::Settlement) that spends the live
//! covenant, submits it over wRPC, waits for the continuation UTXO to confirm, and advances the
//! covenant so the next settlement can spend it. The redeem variant is chosen by the configured
//! [`SettlementMode`](worker::SettlementMode): production (on-chain `OpZkPrecompile` over a real
//! receipt) or dev (chain-anchored seq commit, no precompile, for `RISC0_DEV_MODE` stub-receipt
//! runs).
//!
//! Settlements are serialized: the artifact channel is processed one at a time, and the next
//! settlement is built only after the previous continuation UTXO confirms (so it is spendable).
//!
//! Not yet implemented (structured TODOs in [`worker`]):
//! - **fee bumping**: a settlement that does not confirm within a deadline is currently polled
//!   indefinitely rather than re-submitted at a higher fee;
//! - **done/not-done tracking**: which settlements have landed is not persisted, so a restart
//!   cannot resume mid-chain;
//! - **reorg handling**: a reorg that orphans a settled block is not detected (single-miner /
//!   low-reorg only, matching the aggregate prover's own gap).

mod covenant;
mod worker;

pub use covenant::{
    BuiltSettlement, CovenantAdvance, CovenantState, DEV_COVENANT_BUDGET, bootstrap_dev_covenant,
    bootstrap_real_covenant, bootstrap_redeem, build_dev_settlement, build_settlement,
    build_settlement_for_mode, covenant_from_settlement, dev_bootstrap_redeem,
};
#[cfg(feature = "test-utils")]
pub use worker::AlternationPacer;
pub use worker::{SettlementMode, SettlementWorkerConfig, run};
