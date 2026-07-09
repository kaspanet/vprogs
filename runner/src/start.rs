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
use kaspa_rpc_core::{RpcBlock, api::rpc::RpcApi};
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
        BridgeObservers, BridgeParams, CovenantIdBytes, DepositSpkHash, Elfs, ProvingParams,
        RunnerNode, RunnerStore, SettlementQueue, build_exec_node, build_proving_node,
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

/// Operator-facing start-up failure, as opposed to `ConfigError` (which is about parsing the
/// config): these are about the requested start mode not matching on-disk / supplied state.
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

/// Resolves the effective start mode: explicit when set, else resume if the data dir already holds
/// a covenant identity, else fresh.
fn effective_mode(cfg: &RunnerConfig, persisted: &PersistedState) -> StartMode {
    cfg.start_mode.unwrap_or({
        if persisted.covenant_id.is_some() { StartMode::Resume } else { StartMode::Fresh }
    })
}

/// The resolved start-up inputs a node builder needs: the config, the connected client, the prover
/// identity, the lane wiring, the guest ELFs, the deposit-address derivation, the start mode, and
/// the mutable persisted state.
struct StartContext<'a, F> {
    /// Resolved runner configuration.
    cfg: &'a RunnerConfig,
    /// Connected wRPC client for the remote node.
    client: &'a KaspaRpcClient,
    /// Consensus params for the target network.
    params: &'a Params,
    /// Fee / bootstrap keypair derived from `cfg.private_key`.
    keypair: Keypair,
    /// Lane subnetwork this runner routes onto.
    lane_subnet: SubnetworkId,
    /// Lane key derived from `lane_subnet`.
    lane_key: Hash,
    /// Guest ELF images the backend pins.
    elfs: Elfs<'a>,
    /// The program's deposit-address derivation, applied once to the resolved covenant id.
    deposit_spk_hash: F,
    /// Resolved start mode: fresh bootstrap, resume, or catch-up.
    mode: StartMode,
    /// Persisted identity and bootstrap anchors, updated as start-up resolves them.
    persisted: &'a mut PersistedState,
}

/// Connect-and-run: resolve identity + start mode, build the node and (in prove mode) the settler.
/// Does not issue any action/activity transactions and does not block; the caller owns the handles.
///
/// `deposit_spk_hash` maps the covenant id this node settles onto the deposit address the program's
/// policy requires; a program that credits no L1 deposits returns `[0u8; 32]`. It is applied once,
/// after the covenant is resolved, and only when `cfg.prove` is set. A fresh node mints its
/// covenant during this call, so the address cannot be supplied as a ready value.
// `FnOnce` rather than a plain `fn` so a policy can capture what it needs (a treasury key, a config
// handle) instead of deriving everything from the covenant id.
//
// Three ways a wrong derivation escapes start-up:
//
// - `start_exec` drops it unused, so a wrong one stays invisible until the node runs in prove mode.
// - It is not validated here. Every depositing transaction commits its own address, and the batch
//   circuit aborts on the mismatch, leaving the bundle unprovable and the covenant stuck.
// - `[0u8; 32]` is the no-deposit sentinel, not a default. The circuit skips its carry check on it,
//   so a deposit-crediting program that returns zero looks healthy until the first deposit lands.
pub async fn start_runner<F>(
    cfg: &RunnerConfig,
    client: &KaspaRpcClient,
    params: &Params,
    elfs: Elfs<'_>,
    deposit_spk_hash: F,
) -> Result<RunnerHandles, StartError>
where
    F: FnOnce(&CovenantIdBytes) -> DepositSpkHash,
{
    let keypair = Keypair::from_secret_key(secp256k1::SECP256K1, &cfg.private_key);

    // --- resolve lane id: storage > config > random ---
    let mut persisted = PersistedState::load(&cfg.data_dir);
    let mode = effective_mode(cfg, &persisted);
    let lane_id = persisted.lane_id.or(cfg.lane_id).unwrap_or_else(|| fastrand::u32(1000..));
    persisted.lane_id = Some(lane_id);
    let lane_subnet = SubnetworkId::from_namespace(lane_id.to_be_bytes());
    let lane_key = lane_key(lane_subnet.as_bytes());
    log::info!("lane id={lane_id} subnetwork={lane_subnet} mode={mode:?}");

    let ctx = StartContext {
        cfg,
        client,
        params,
        keypair,
        lane_subnet,
        lane_key,
        elfs,
        deposit_spk_hash,
        mode,
        persisted: &mut persisted,
    };
    if cfg.prove {
        let (node, settler, covenant_id) = start_settlement(ctx).await?;
        Ok(RunnerHandles { node, settler: Some(settler), lane_id, lane_subnet, covenant_id })
    } else {
        let (node, covenant_id) = start_exec(ctx).await?;
        Ok(RunnerHandles { node, settler: None, lane_id, lane_subnet, covenant_id })
    }
}

/// Builds the execution-only node: resolve or dev-bootstrap a covenant per the start mode, then a
/// `Node` with no proving. The bridge tracks the covenant's settlements; nothing here settles.
async fn start_exec<F>(ctx: StartContext<'_, F>) -> Result<(RunnerNode, Hash), StartError> {
    // Exec mode runs no prover, so the deposit-address derivation is dropped unused.
    let StartContext {
        cfg,
        client,
        params,
        keypair,
        lane_subnet,
        lane_key,
        elfs,
        deposit_spk_hash: _,
        mode,
        persisted,
    } = ctx;

    // Resolve the covenant id and the seed block per mode. `seed` is the L1 block the bridge
    // rebuilds decoded state forward from; a catch-up that seeds only `seed_depth` below the
    // tip misses lane history and reconstructs wrong state, so catch-up requires an explicit
    // deploy block just like the settlement path.
    let (covenant_id, seed) = match mode {
        StartMode::Resume => {
            let covenant_id = persisted
                .covenant_hash()
                .ok_or_else(|| StartError::NoPersistedState(cfg.data_dir.clone()))?;
            (covenant_id, persisted.bootstrap_block().or(cfg.start_from))
        }
        StartMode::Catchup => {
            let covenant_id = cfg.covenant_id.ok_or(StartError::NoCovenantId)?;
            let seed = persisted
                .bootstrap_block()
                .or(cfg.start_from)
                .ok_or(StartError::CatchupNeedsStartFrom(covenant_id))?;
            (covenant_id, Some(seed))
        }
        StartMode::Fresh => {
            if persisted.covenant_id.is_some() {
                return Err(StartError::DataDirNotClean(cfg.data_dir.clone()));
            }
            // Capture the node's selected tip before bootstrap: a real chain block at or just
            // before the deploy block, seeded so a later resume replays forward from
            // the deploy.
            let seed_block = client.get_block_dag_info().await.expect("get_block_dag_info").sink;
            let wallet = Wallet::new(client, params, keypair);
            log::info!("bootstrapping dev covenant; issuer address {}", wallet.address());
            let (covenant, bootstrap_txid) =
                bootstrap_dev_covenant(&wallet, lane_key, COVENANT_VALUE).await;
            persisted.bootstrap_txid = Some(bootstrap_txid.to_string());
            log::info!("covenant {} bootstrapped (tx {})", covenant.covenant_id, bootstrap_txid);
            (covenant.covenant_id, Some(seed_block))
        }
    };
    // Persist the resolved id, any supplied bootstrap anchor, and the seed block so a catch-up (or
    // a fresh deploy) becomes a plain resume on the next restart without re-supplying them.
    persisted.covenant_id.get_or_insert_with(|| covenant_id.to_string());
    if let Some(txid) = cfg.bootstrap_txid {
        persisted.bootstrap_txid.get_or_insert_with(|| txid.to_string());
    }
    if let Some(seed) = seed {
        persisted.bootstrap_block_hash.get_or_insert_with(|| seed.to_string());
    }
    persisted.save(&cfg.data_dir);

    // Seed from the persisted deploy block if we have one, else from the supplied seed.
    let start_from = persisted.bootstrap_block().or(cfg.start_from);
    // Seed the bridge with reorg headroom: pin the anchor only if it is already deep, else seed
    // seed_depth below the sink (see `resolve_bridge_seed`).
    let tip_daa = client.get_block_dag_info().await.expect("get_block_dag_info").virtual_daa_score;
    let bridge_seed = resolve_bridge_seed(client, start_from, cfg.seed_depth, tip_daa).await;

    let store = RunnerStore::open(cfg.data_dir.join("db"));
    let node = build_exec_node(
        elfs,
        store,
        bridge_params(
            cfg,
            lane_subnet,
            covenant_id,
            params,
            bridge_seed,
            BridgeObservers::default(),
        ),
    );
    Ok((node, covenant_id))
}

/// Builds the proving + settlement node per the start mode: fresh bootstrap (dev-pins under
/// `RISC0_DEV_MODE`, real-pins otherwise), resume from persisted identity, or catch up to an
/// existing covenant (reconstructing its initial state). Wires the batch prover over the remote
/// node and spawns the settler on the node's batch sink.
async fn start_settlement<F>(
    ctx: StartContext<'_, F>,
) -> Result<(RunnerNode, (JoinHandle<()>, AtomicAsyncLatch), Hash), StartError>
where
    F: FnOnce(&CovenantIdBytes) -> DepositSpkHash,
{
    let StartContext {
        cfg,
        client,
        params,
        keypair,
        lane_subnet,
        lane_key,
        elfs,
        deposit_spk_hash,
        mode,
        persisted,
    } = ctx;

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
    // the covenant, which the seed contract requires. The virtual DAA is captured alongside to size
    // the bridge's reorg headroom (see `resolve_bridge_seed`) and to target the sync reporter.
    let dag_info = client.get_block_dag_info().await.expect("get_block_dag_info");
    let seed_block = dag_info.sink;
    let tip_daa = dag_info.virtual_daa_score;

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
            // loses pre-seed lane history and corrupts the reconstructed seq commit. There is no
            // RPC to resolve a tx's containing block, so fail fast instead of seeding
            // from the wrong height.
            let start_from = persisted
                .bootstrap_block()
                .or(cfg.start_from)
                .ok_or(StartError::CatchupNeedsStartFrom(covenant_id))?;
            Some((
                covenant_id,
                Some(start_from),
                cfg.bootstrap_txid.or_else(|| persisted.bootstrap_txid()),
            ))
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
        let (_redeem, spk) = if dev {
            dev_bootstrap_redeem(&lane_key)
        } else {
            bootstrap_redeem(&backend, &lane_key)
        };
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
        log::info!(
            "settlement mode (dev): bootstrapping dev-pins covenant; issuer {}",
            wallet.address()
        );
        let (covenant, txid) = bootstrap_dev_covenant(&wallet, lane_key, COVENANT_VALUE).await;
        (covenant, Some(txid))
    } else {
        log::info!(
            "settlement mode: bootstrapping real-pins covenant; issuer {}",
            wallet.address()
        );
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
    // The bridge replays from the pruning point and publishes its tip DAA here; a reporter task
    // polls it against the bootstrap's DAA to log how far the catch-up has progressed.
    let tip_daa_obs = Arc::new(AtomicU64::new(0));
    // Live settlement channel: the bridge (writer) publishes the covenant's last on-chain
    // settlement here; the settler (reader) detects a competitor advancing past its in-memory
    // tip.
    let (settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    // Seed the bridge with reorg headroom: pin the anchor only if it is already deep, else seed
    // seed_depth below the sink. The settler keeps the unmodified `start_from` (its own
    // resume/adopt semantics), so this only affects where the bridge roots its chain.
    let bridge_seed = resolve_bridge_seed(client, start_from, cfg.seed_depth, tip_daa).await;
    let node = build_proving_node(
        elfs,
        store,
        bridge_params(
            cfg,
            lane_subnet,
            covenant_id,
            params,
            bridge_seed,
            BridgeObservers { tip_daa: Some(tip_daa_obs.clone()), settlement: Some(settlement_tx) },
        ),
        ProvingParams {
            covenant_id,
            // Bind the pin to the covenant this node actually settles: minted just above in fresh
            // mode, read from persisted state or config otherwise.
            deposit_spk_hash: deposit_spk_hash(&covenant_id.as_bytes()),
            lane_key,
            client: client.clone(),
            sink: queue.clone(),
            bundle_size: 1..=usize::MAX,
            settlement_rx: Some(settlement_rx.clone()),
        },
    );
    // Target the bridge replays toward: the node's virtual DAA captured before bootstrap. The
    // reporter loop only reads the tip atomic.
    spawn_sync_reporter(tip_daa_obs, tip_daa);

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

/// Resolves the explicit block the bridge roots its fresh chain at, decoupled from the settler's
/// `start_from`. Always returns an explicit block (never a "seed `seed_depth` below the sink
/// yourself" signal), so the bridge takes the fast `seed_from_block` path rather than its deep
/// sequential `seed_from_recent` walk.
///
/// Pins `anchor` directly when it is already at least `seed_depth` DAA below the tip (reorg-safe; a
/// deep catch-up loses no history). When `anchor` is near the tip (a fresh bootstrap, whose
/// just-minted deploy block has no headroom yet), resolves an explicit root `seed_depth` DAA below
/// the sink instead. Returns `None` only when there is no anchor at all. The near-tip case must not
/// resolve to `None`: that routes the worker into the `seed_from_recent` walk, which stalls against
/// a live node and never advances.
async fn resolve_bridge_seed<R: RpcApi>(
    client: &R,
    anchor: Option<Hash>,
    seed_depth: u64,
    tip_daa: u64,
) -> Option<Hash> {
    let anchor = anchor?;
    match get_block_with_retry(client, anchor).await {
        // Already at least `seed_depth` below the tip: reorg-safe, so pin the exact deploy block
        // (no history lost).
        Some(block) if tip_daa.saturating_sub(block.header.daa_score) >= seed_depth => Some(anchor),
        // Near-tip anchor (a fresh bootstrap): resolve an explicit root `seed_depth` below the sink
        // for headroom. Resolving it here, once at start-up, keeps that walk off the worker's
        // reconnect path. Falls back to pinning the anchor if it cannot resolve.
        Some(_) => seed_root_below_sink(client, seed_depth).await.or(Some(anchor)),
        // Persistently unresolvable (e.g. pruned): genuinely deep and reorg-safe, so pin it rather
        // than silently reseeding elsewhere.
        None => Some(anchor),
    }
}

/// Returns an explicit, reorg-safe seed root at least `depth` DAA below the current sink, found by
/// walking the selected-parent chain back from the sink, or `None` if it cannot be resolved. Each
/// `get_block` is bounded (retry-with-backoff), so a transient or hung RPC cannot stall start-up.
/// Stops early at the chain base (a block whose selected parent is itself).
async fn seed_root_below_sink<R: RpcApi>(client: &R, depth: u64) -> Option<Hash> {
    let dag = client.get_block_dag_info().await.ok()?;
    let (tip_daa, mut hash) = (dag.virtual_daa_score, dag.sink);
    loop {
        let block = get_block_with_retry(client, hash).await?;
        let parent = block.verbose_data?.selected_parent_hash;
        // Deep enough for the requested headroom, or the chain base (selected parent is itself).
        if tip_daa.saturating_sub(block.header.daa_score) >= depth || parent == hash {
            return Some(hash);
        }
        hash = parent;
    }
}

/// Fetches a block, retrying transient RPC failures with a bounded backoff. Returns `None` once the
/// attempts are exhausted (a persistent failure, e.g. a pruned block or an unresponsive node). A
/// live node times out transiently, so a single blip must not be treated as terminal.
async fn get_block_with_retry<R: RpcApi>(client: &R, hash: Hash) -> Option<RpcBlock> {
    const MAX_ATTEMPTS: u32 = 10;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
    for attempt in 1..=MAX_ATTEMPTS {
        match client.get_block(hash, false).await {
            Ok(block) => return Some(block),
            Err(e) if attempt < MAX_ATTEMPTS => {
                log::warn!(
                    "bridge seed: get_block {hash} failed (attempt {attempt}/{MAX_ATTEMPTS}, retrying): {e}"
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(e) => {
                log::warn!(
                    "bridge seed: get_block {hash} failed after {MAX_ATTEMPTS} attempts: {e}"
                );
                return None;
            }
        }
    }
    None
}

/// The bridge wiring for either mode, pointed at the remote node's lane + covenant. `bridge_seed`
/// is the resolved root block ([`resolve_bridge_seed`]): an explicit reorg-safe root to pin, or
/// `None` (no anchor at all) to fall back to the bridge's own `seed_depth`-below-sink seeding.
fn bridge_params(
    cfg: &RunnerConfig,
    lane_subnet: SubnetworkId,
    covenant_id: Hash,
    params: &Params,
    bridge_seed: Option<Hash>,
    observers: BridgeObservers,
) -> BridgeParams {
    BridgeParams {
        url: cfg.wrpc_url.clone(),
        network_id: cfg.network_id,
        lane_subnet,
        covenant_id,
        finality_depth: params.finality_depth(),
        seed_depth: cfg.seed_depth,
        start_from: bridge_seed,
        observers,
    }
}

#[cfg(test)]
mod tests {
    use kaspa_consensus_core::network::{NetworkId, NetworkType};
    use vprogs_node_test_utils::L1Node;

    use super::*;

    /// A fresh-bootstrap node resolves its bridge seed the instant it mints the deploy block, so
    /// the anchor sits at the tip. `resolve_bridge_seed` must still hand the bridge an
    /// *explicit* root with reorg headroom (a real block `seed_depth` below the sink) rather
    /// than `None` - `None` routes the worker into the deep sequential `seed_from_recent` walk,
    /// which stalls against a live node and never advances (the fresh-node-A regression).
    #[tokio::test]
    async fn resolve_bridge_seed_returns_deep_root_for_near_tip_anchor() {
        let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
        // A chain deeper than seed_depth, so a headroom root genuinely exists below the tip.
        l1.mine_blocks(20).await;
        let client = l1.grpc_client();
        let dag = client.get_block_dag_info().await.unwrap();
        let (sink, tip_daa) = (dag.sink, dag.virtual_daa_score);
        let seed_depth = 5;

        // The anchor is the sink itself: tip_daa - anchor_daa < seed_depth (node A at bootstrap).
        let root = resolve_bridge_seed(client, Some(sink), seed_depth, tip_daa)
            .await
            .expect("a near-tip anchor must resolve to an explicit deep seed, not None");

        assert_ne!(root, sink, "the seed root must sit below the sink for reorg headroom");
        let root_daa = client.get_block(root, false).await.unwrap().header.daa_score;
        assert!(
            tip_daa.saturating_sub(root_daa) >= seed_depth,
            "seed root must be >= seed_depth below the tip (was {} below)",
            tip_daa.saturating_sub(root_daa),
        );
    }

    /// A catch-up node whose anchor is already `seed_depth` below the tip keeps pinning the exact
    /// deploy block (no history lost, and it is deep enough to be reorg-safe).
    #[tokio::test]
    async fn resolve_bridge_seed_pins_already_deep_anchor() {
        let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
        let mined = l1.mine_blocks(20).await;
        let client = l1.grpc_client();
        let tip_daa = client.get_block_dag_info().await.unwrap().virtual_daa_score;
        let seed_depth = 3;

        let deep_anchor = mined[5];
        let deep_daa = client.get_block(deep_anchor, false).await.unwrap().header.daa_score;
        assert!(tip_daa - deep_daa >= seed_depth, "test setup: anchor must be deep");

        assert_eq!(
            resolve_bridge_seed(client, Some(deep_anchor), seed_depth, tip_daa).await,
            Some(deep_anchor),
        );
    }
}
