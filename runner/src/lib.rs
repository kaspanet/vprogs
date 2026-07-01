//! vprogs-runner: a program-agnostic daemon engine that follows a Kaspa L1 lane, executes an
//! arbitrary guest program over its transactions, and optionally proves and settles the results.
//!
//! It is the reusable core extracted from the tn10-flow proof-of-concept, minus any action/activity
//! transaction issuing: the runner only fetches, executes, and (optionally) proves + settles.
//! Issuing lane transactions is a caller concern (see the examples).
//!
//! Two run modes, selected by [`RunnerConfig::prove`]:
//! - **Execution-only**: a framework node with no proving that tracks decoded state, reorgs, and
//!   settlements.
//! - **Proving + settlement**: the full transaction/batch/aggregate proving stack, with a
//!   settlement worker landing each proved bundle on chain. Real proofs need a GPU (the `cuda`
//!   feature) without `RISC0_DEV_MODE`; otherwise stub proofs settle against the dev redeem.
//!
//! Three explicit start modes ([`StartMode`]): `Fresh` (bootstrap a covenant), `Resume` (from
//! persisted identity), `Catchup` (join an existing covenant from a known deploy block).

mod config;
mod lane;
mod node;
mod persistence;
mod report;
mod start;
mod wrpc;

pub use config::{ConfigError, OwnedElfs, RawConfig, RunnerConfig, StartMode, parse_network};
use kaspa_consensus_core::config::params::Params;
pub use node::{
    BridgeObservers, BridgeParams, Elfs, ProvingParams, RunnerNode, RunnerStore, RunnerVm,
    SettlementQueue, build_exec_node, build_proving_node,
};
pub use persistence::PersistedState;
pub use start::{RunnerHandles, StartError, start_runner};
pub use wrpc::connect_wrpc;

/// Top-level runner entry point for the binary: connect, load the guest ELFs named by `config`, and
/// start the node (and, in prove mode, the settler). Returns live handles the caller keeps alive.
///
/// Examples and tests that already hold ELF bytes should call [`start_runner`] directly instead of
/// going through the config's ELF paths.
pub async fn run(config: RunnerConfig) -> Result<RunnerHandles, RunError> {
    let elfs = config.load_elfs()?;
    let client = connect_wrpc(&config.wrpc_url, config.network_id).await;
    log::info!("connected to {}", config.wrpc_url);
    // Network params, used off-chain only for mass calculation and lane-key derivation; never
    // pushed to the node (the remote node runs its own params, with the covenant forks active).
    let params = Params::from(config.network_id);
    let handles = start_runner(&config, &client, &params, elfs.as_elfs()).await?;
    Ok(handles)
}

/// Failure from the top-level [`run`] entry point.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The config could not be resolved or its ELFs could not be read.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The requested start mode did not match on-disk / supplied state.
    #[error(transparent)]
    Start(#[from] StartError),
}
