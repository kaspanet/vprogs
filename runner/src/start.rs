//! Start-up orchestration: resolve identity + start mode, build the node (exec or
//! proving+settlement), and spawn the sync reporter and (in prove mode) the settlement worker.
//!
//! This is the generic engine tn10-flow's `main` used to hold inline, minus the activity issuer:
//! the runner only fetches, executes, and optionally proves + settles. Issuing action transactions
//! is left to the caller (the examples).

use std::sync::{Arc, atomic::AtomicU64};

use kaspa_consensus_core::{
    config::params::Params, constants::SOMPI_PER_KASPA, subnets::SubnetworkId,
    tx::TransactionOutpoint,
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_seq_commit::hashing::lane_key;
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use tokio::{sync::watch, task::JoinHandle};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_l1_types::SettlementInfo;
use vprogs_l1_wallet::Wallet;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_settler::{
    CovenantState, SettlementMode, SettlementWorkerConfig, bootstrap_dev_covenant,
    bootstrap_real_covenant, bootstrap_redeem, dev_bootstrap_redeem, run as run_settlement_worker,
};
use vprogs_zk_backend_risc0_test_suite::dev_mode_enabled;

use crate::{
    config::{RunnerConfig, StartMode},
    node::{
        BridgeObservers, BridgeParams, Elfs, ProvingParams, RunnerNode, RunnerStore, SettlementQueue,
        build_exec_node, build_proving_node,
    },
    persistence::PersistedState,
    report::spawn_sync_reporter,
};

/// Value locked in the covenant UTXO at bootstrap (1 KAS), matching the e2e tests.
const COVENANT_VALUE: u64 = SOMPI_PER_KASPA;

/// Live handles the runner returns to its caller. The caller keeps `node` alive (dropping it shuts
/// the flow down) and, in prove mode, owns the settler handle to await it.
pub struct RunnerHandles {
    /// The framework node driving the flow. Kept alive by the caller.
    pub node: RunnerNode,
    /// Present only in prove mode: the settlement worker's join handle and its shutdown latch.
    pub settler: Option<(JoinHandle<()>, AtomicAsyncLatch)>,
    /// The resolved lane id (subnetwork namespace) the runner follows.
    pub lane_id: u32,
    /// The resolved lane subnetwork.
    pub lane_subnet: SubnetworkId,
    /// The resolved covenant id the runner follows / settles.
    pub covenant_id: Hash,
}

/// Operator-facing start-up failure. Distinct from `ConfigError` (which is about parsing the config)
/// — these are about the requested start mode not matching on-disk / supplied state.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// Resume was requested but no persisted covenant identity exists in the data dir.
    #[error("resume requested but no persisted covenant in {0}; bootstrap fresh or catch up")]
    NoPersistedState(std::path::PathBuf),
    /// Catch-up was requested without a covenant id to join.
    #[error("catch-up requested but no covenant_id supplied")]
    NoCovenantId,
    /// Catch-up in prove mode was requested without the deploy block; seeding only `seed_depth`
    /// below the sink loses pre-seed lane history and corrupts the reconstructed seq commit.
    #[error(
        "catch-up to covenant {0} requires start_from (the deploy block); seeding only seed_depth \
         below the sink loses pre-seed lane history and corrupts the reconstructed seq_commit"
    )]
    CatchupNeedsStartFrom(Hash),
    /// Fresh bootstrap was requested but the data dir already holds a covenant identity.
    #[error("fresh bootstrap requested but {0} already holds persisted state; use resume instead")]
    DataDirNotClean(std::path::PathBuf),
}

/// Resolves the effective start mode: explicit when set, else resume if the data dir already holds a
/// covenant identity, else fresh.
fn effective_mode(cfg: &RunnerConfig, persisted: &PersistedState) -> StartMode {
    cfg.start_mode.unwrap_or({
        if persisted.covenant_id.is_some() { StartMode::Resume } else { StartMode::Fresh }
    })
}

/// Connect-and-run: resolve identity + start mode, build the node and (in prove mode) the settler.
/// Does not issue any action/activity transactions and does not block; the caller owns the handles.
pub async fn start_runner(
    cfg: &RunnerConfig,
    client: &KaspaRpcClient,
    params: &Params,
    elfs: Elfs<'_>,
) -> Result<RunnerHandles, StartError> {
    let keypair = Keypair::from_secret_key(secp256k1::SECP256K1, &cfg.private_key);

    // --- resolve lane id: storage > config > random ---
    let mut persisted = PersistedState::load(&cfg.data_dir);
    let mode = effective_mode(cfg, &persisted);
    let lane_id = persisted.lane_id.or(cfg.lane_id).unwrap_or_else(|| fastrand::u32(1000..));
    persisted.lane_id = Some(lane_id);
    let lane_subnet = SubnetworkId::from_namespace(lane_id.to_be_bytes());
    let lane_key = lane_key(lane_subnet.as_bytes());
    log::info!("lane id={lane_id} subnetwork={lane_subnet} mode={mode:?}");

    if cfg.prove {
        let (node, settler, covenant_id) =
            start_settlement(cfg, client, params, keypair, lane_subnet, lane_key, elfs, mode, &mut persisted)
                .await?;
        Ok(RunnerHandles { node, settler: Some(settler), lane_id, lane_subnet, covenant_id })
    } else {
        let (node, covenant_id) =
            start_exec(cfg, client, params, keypair, lane_subnet, lane_key, elfs, mode, &mut persisted)
                .await?;
        Ok(RunnerHandles { node, settler: None, lane_id, lane_subnet, covenant_id })
    }
}

/// Builds the execution-only node: resolve or dev-bootstrap a covenant per the start mode, then a
/// `Node` with no proving. The bridge tracks the covenant's settlements; nothing here settles.
#[allow(clippy::too_many_arguments)]
async fn start_exec(
    cfg: &RunnerConfig,
    client: &KaspaRpcClient,
    params: &Params,
    keypair: Keypair,
    lane_subnet: SubnetworkId,
    lane_key: Hash,
    elfs: Elfs<'_>,
    mode: StartMode,
    persisted: &mut PersistedState,
) -> Result<(RunnerNode, Hash), StartError> {
    let covenant_id = match mode {
        StartMode::Resume => persisted
            .covenant_hash()
            .ok_or_else(|| StartError::NoPersistedState(cfg.data_dir.clone()))?,
        StartMode::Catchup => cfg.covenant_id.ok_or(StartError::NoCovenantId)?,
        StartMode::Fresh => {
            if persisted.covenant_id.is_some() {
                return Err(StartError::DataDirNotClean(cfg.data_dir.clone()));
            }
            let wallet = Wallet::new(client, params, keypair);
            log::info!("bootstrapping dev covenant; issuer address {}", wallet.address());
            let (covenant, bootstrap_txid) =
                bootstrap_dev_covenant(&wallet, lane_key, COVENANT_VALUE).await;
            persisted.bootstrap_txid = Some(bootstrap_txid.to_string());
            log::info!("covenant {} bootstrapped (tx {})", covenant.covenant_id, bootstrap_txid);
            covenant.covenant_id
        }
    };
    // Persist the resolved id and any supplied bootstrap anchor so a catch-up becomes a plain resume
    // on the next restart.
    persisted.covenant_id.get_or_insert_with(|| covenant_id.to_string());
    if let Some(txid) = cfg.bootstrap_txid {
        persisted.bootstrap_txid.get_or_insert_with(|| txid.to_string());
    }
    persisted.save(&cfg.data_dir);

    // Seed from the persisted deploy block if we have one, else from the supplied seed.
    let start_from = persisted.bootstrap_block().or(cfg.start_from);

    let store = RunnerStore::open(cfg.data_dir.join("db"));
    let node = build_exec_node(
        elfs,
        store,
        bridge_params(cfg, lane_subnet, covenant_id, params, start_from, BridgeObservers::default()),
    );
    Ok((node, covenant_id))
}

/// Builds the proving + settlement node per the start mode: fresh bootstrap (dev-pins under
/// `RISC0_DEV_MODE`, real-pins otherwise), resume from persisted identity, or catch up to an existing
/// covenant (reconstructing its initial state). Wires the batch prover over the remote node and
/// spawns the settler on the node's batch sink.
#[allow(clippy::too_many_arguments)]
async fn start_settlement(
    cfg: &RunnerConfig,
    client: &KaspaRpcClient,
    params: &Params,
    keypair: Keypair,
    lane_subnet: SubnetworkId,
    lane_key: Hash,
    elfs: Elfs<'_>,
    mode: StartMode,
    persisted: &mut PersistedState,
) -> Result<(RunnerNode, (JoinHandle<()>, AtomicAsyncLatch), Hash), StartError> {
    let backend = Backend::new(elfs.program, elfs.batch, elfs.aggregator, ProofType::Succinct);
    let wallet = Wallet::new(client, params, keypair);
    // Under `RISC0_DEV_MODE` the prover emits stub receipts the production `OpZkPrecompile` would
    // reject, so settle against the dev redeem (chain-anchored seq commit, no precompile); a real
    // (CUDA, non-dev) run settles against the production redeem. Same operating contract the tests
    // gate on.
    let dev = dev_mode_enabled();
    let settlement_mode = if dev { SettlementMode::Dev } else { SettlementMode::Production };
    // The node's selected tip right now: a real chain block at or just before any deploy block we
    // are about to mint. Captured before bootstrap so a fresh deploy seeds the bridge at or before
    // the covenant, which the seed contract requires.
    let seed_block = client.get_block_dag_info().await.expect("get_block_dag_info").sink;

    // Resolve the covenant identity + reconstruction inputs from the explicit start mode.
    let resolved = match mode {
        StartMode::Resume => Some((
            persisted
                .covenant_hash()
                .ok_or_else(|| StartError::NoPersistedState(cfg.data_dir.clone()))?,
            // Resume carries a persisted deploy block, so no operator start_from is required.
            persisted.bootstrap_block().or(cfg.start_from),
            cfg.bootstrap_txid.or_else(|| persisted.bootstrap_txid()),
        )),
        StartMode::Catchup => {
            let covenant_id = cfg.covenant_id.ok_or(StartError::NoCovenantId)?;
            // A fresh catch-up without a deploy block seeds only `seed_depth` below the sink, which
            // loses pre-seed lane history and corrupts the reconstructed seq commit. There is no RPC
            // to resolve a tx's containing block, so fail fast instead of seeding from the wrong
            // height.
            let start_from = persisted
                .bootstrap_block()
                .or(cfg.start_from)
                .ok_or(StartError::CatchupNeedsStartFrom(covenant_id))?;
            Some((covenant_id, Some(start_from), cfg.bootstrap_txid.or_else(|| persisted.bootstrap_txid())))
        }
        StartMode::Fresh => {
            if persisted.covenant_id.is_some() {
                return Err(StartError::DataDirNotClean(cfg.data_dir.clone()));
            }
            None
        }
    };

    let (covenant, bootstrap_txid) = if let Some((covenant_id, _seed, bootstrap_txid)) = resolved {
        // Same redeem builder bootstrap uses, so the reconstructed P2SH SPK matches the on-chain
        // covenant UTXO (asserted at the first settlement).
        let (_redeem, spk) =
            if dev { dev_bootstrap_redeem(&lane_key) } else { bootstrap_redeem(&backend, &lane_key) };
        // Real bootstrap outpoint if known, so a resumed never-settled covenant still confirms its
        // real bootstrap UTXO; else a placeholder the settler replaces on adoption when the
        // bootstrap is already spent.
        let outpoint = match bootstrap_txid {
            Some(txid) => {
                log::info!("resolving existing covenant {covenant_id} (bootstrap tx {txid})");
                TransactionOutpoint::new(txid, 0)
            }
            None => {
                log::info!(
                    "resolving existing covenant {covenant_id} without a bootstrap txid; the \
                     settler adopts the on-chain tip if the bootstrap is already spent"
                );
                TransactionOutpoint::new(covenant_id, 0)
            }
        };
        let covenant = CovenantState {
            covenant_id,
            state: EMPTY_HASH,
            lane_tip: Hash::default(),
            outpoint,
            spk,
            value: COVENANT_VALUE,
            daa_score: 0,
        };
        (covenant, bootstrap_txid)
    } else if dev {
        log::info!("settlement mode (dev): bootstrapping dev-pins covenant; issuer {}", wallet.address());
        let (covenant, txid) = bootstrap_dev_covenant(&wallet, lane_key, COVENANT_VALUE).await;
        (covenant, Some(txid))
    } else {
        log::info!("settlement mode: bootstrapping real-pins covenant; issuer {}", wallet.address());
        let (covenant, txid) =
            bootstrap_real_covenant(&wallet, &backend, lane_key, COVENANT_VALUE).await;
        (covenant, Some(txid))
    };
    let covenant_id = covenant.covenant_id;

    // Persist the resolved id, the bootstrap anchor (when known), and the seed block. A resolved
    // covenant seeds from its deploy block; a fresh deploy seeds from the captured sink.
    persisted.covenant_id = Some(covenant_id.to_string());
    if let Some(txid) = bootstrap_txid {
        persisted.bootstrap_txid.get_or_insert_with(|| txid.to_string());
    }
    let seed_for_resume = match &resolved {
        Some((_, seed, _)) => *seed,
        None => Some(seed_block),
    };
    if let Some(seed) = seed_for_resume {
        persisted.bootstrap_block_hash.get_or_insert_with(|| seed.to_string());
    }
    persisted.save(&cfg.data_dir);
    log::info!("covenant {covenant_id} ready (bootstrap tx {bootstrap_txid:?})");

    let start_from = persisted.bootstrap_block().or(cfg.start_from);

    // The in-process aggregate prover publishes each proved bundle handle onto this queue; the
    // settlement worker pops from it and settles on chain.
    let queue = SettlementQueue::new();
    let store = RunnerStore::open(cfg.data_dir.join("db"));
    // The bridge replays from the pruning point and publishes its tip DAA here; a reporter task polls
    // it against the bootstrap's DAA to log how far the catch-up has progressed.
    let tip_daa = Arc::new(AtomicU64::new(0));
    // Live settlement channel: the bridge (writer) publishes the covenant's last on-chain settlement
    // here; the settler (reader) detects a competitor advancing past its in-memory tip.
    let (settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    let node = build_proving_node(
        elfs,
        store,
        bridge_params(
            cfg,
            lane_subnet,
            covenant_id,
            params,
            start_from,
            BridgeObservers { tip_daa: Some(tip_daa.clone()), settlement: Some(settlement_tx) },
        ),
        ProvingParams {
            covenant_id,
            lane_key,
            client: client.clone(),
            sink: queue.clone(),
            bundle_size: 1..=usize::MAX,
            settlement_rx: Some(settlement_rx.clone()),
        },
    );
    // Target the bridge replays toward: the node's virtual DAA now. Captured once; the reporter loop
    // only reads the tip atomic.
    let target_daa =
        client.get_block_dag_info().await.expect("get_block_dag_info").virtual_daa_score;
    spawn_sync_reporter(tip_daa, target_daa);

    // The settlement worker drains bundle handles off the queue until `main` opens this latch on a
    // signal, or it hits a fatal error (a rejected settlement or a confirmation timeout).
    let shutdown = AtomicAsyncLatch::new();
    let settler = tokio::spawn(run_settlement_worker(
        queue,
        SettlementWorkerConfig {
            client: client.clone(),
            params: params.clone(),
            keypair,
            lane_key,
            covenant_id,
            start_from,
            backend,
            mode: settlement_mode,
            settlement: settlement_rx,
            submit_jitter: None,
            #[cfg(feature = "test-utils")]
            alternation: None,
        },
        covenant,
        shutdown.clone(),
    ));
    Ok((node, (settler, shutdown), covenant_id))
}

/// The bridge wiring for either mode, pointed at the remote node's lane + covenant. `start_from` is
/// the resolved seed block (persisted bootstrap block, else supplied), which the bridge seeds its
/// fresh-chain root at so a resume/catch-up replays L1 forward from the deploy.
fn bridge_params(
    cfg: &RunnerConfig,
    lane_subnet: SubnetworkId,
    covenant_id: Hash,
    params: &Params,
    start_from: Option<Hash>,
    observers: BridgeObservers,
) -> BridgeParams {
    BridgeParams {
        url: cfg.wrpc_url.clone(),
        network_id: cfg.network_id,
        lane_subnet,
        covenant_id,
        finality_depth: params.finality_depth(),
        seed_depth: cfg.seed_depth,
        start_from,
        observers,
    }
}
