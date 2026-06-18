//! tn10-flow: a proof-of-concept that runs the L2 flow against a remote testnet-10 fork node.
//!
//! One env-driven process: bootstrap a covenant (or reuse a stored one), periodically issue
//! activity transactions on a lane, and follow the chain via the existing [`L1Bridge`], executing
//! the lane's transactions through the same VM the settlement-l1 tests use. It runs in one of two
//! modes:
//!
//! - **Execution-only** (default): a [`Node`](vprogs_node_framework::Node) with
//!   [`ProvingPipeline::None`] that just tracks the decoded state counter, reorgs, and settlements.
//!   Per-block observability is the framework worker's and the Vm's `trace` logs.
//! - **Proving + settlement** (`TN10_SETTLE=1`): the node drives the full proving stack
//!   ([`ProvingPipeline::aggregate`]: transaction + batch + aggregate provers), and the in-process
//!   aggregate prover hands each proved bundle to the [`settlement
//!   worker`](vprogs_zk_backend_risc0_settler), which settles it on chain. Under `RISC0_DEV_MODE`
//!   the prover emits stub receipts and the worker settles against the dev redeem (chain-anchored
//!   seq commit, no `OpZkPrecompile`), so the whole flow runs without a GPU; a real-proof run needs
//!   a GPU (the `cuda` feature, without `RISC0_DEV_MODE`) and settles against the production
//!   redeem.
//!
//! Required env: `TN10_WRPC_URL`, `TN10_PRIVATE_KEY`. See `config.rs` for the full surface.

mod config;
mod daemon;
mod persistence;

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use kaspa_consensus_core::{
    config::params::Params,
    constants::{SOMPI_PER_KASPA, TX_VERSION_TOCCATA},
    network::NetworkId,
    subnets::SubnetworkId,
    tx::TransactionOutpoint,
};
use kaspa_hashes::Hash;
use kaspa_seq_commit::hashing::lane_key;
use kaspa_wrpc_client::prelude::*;
use secp256k1::Keypair;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_wallet::{Wallet, encode_activity_payload};
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_settler::{
    CovenantState, SettlementMode, SettlementWorkerConfig, bootstrap_dev_covenant,
    bootstrap_real_covenant, bootstrap_redeem, dev_bootstrap_redeem, run as run_settlement_worker,
};
use vprogs_zk_backend_risc0_test_suite::{
    batch_aggregator_elf, batch_processor_elf, dev_mode_enabled, transaction_processor_elf,
};

use crate::{
    config::Config,
    daemon::{BridgeParams, Elfs, FlowNode, ProvingParams},
    persistence::PersistedState,
};

/// Value locked in the covenant UTXO at bootstrap (1 TKAS), matching the e2e tests.
const COVENANT_VALUE: u64 = SOMPI_PER_KASPA;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    kaspa_core::log::try_init_logger(
        "info,tn10_flow=info,vprogs_node_framework=trace,vprogs_zk_vm=trace,risc0_zkvm=warn",
    );

    let cfg = Config::from_env();
    let network_id = cfg.network_id;

    // Network params, used off-chain only for mass calculation and lane-key derivation; never
    // pushed to the node (the remote fork node runs its own params, with the covenant forks
    // active).
    let params = Params::from(network_id);
    let client = connect_wrpc(&cfg.wrpc_url, network_id).await;
    log::info!("connected to {}", cfg.wrpc_url);

    // --- resolve lane id: storage > env > random ---
    let mut persisted = PersistedState::load(&cfg.data_dir);
    let lane_id = persisted.lane_id.or(cfg.lane_id_env).unwrap_or_else(|| fastrand::u32(1000..));
    persisted.lane_id = Some(lane_id);
    let lane_subnet = SubnetworkId::from_namespace(lane_id.to_be_bytes());
    let lane_key = lane_key(lane_subnet.as_bytes());
    log::info!("lane id={lane_id} subnetwork={lane_subnet}");

    let keypair = Keypair::from_secret_key(secp256k1::SECP256K1, &cfg.private_key);
    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { transaction: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };

    // Build the node (kept alive until we return; dropping it shuts the flow down). Settlement mode
    // additionally bootstraps a real-pins covenant and spawns the settler (whose handle we keep),
    // which drains the batch sink the node forwards to it.
    let (_node, settler): (FlowNode, Option<(tokio::task::JoinHandle<()>, AtomicAsyncLatch)>) =
        if cfg.enable_settlements {
            let (node, settler, shutdown) = start_settlement(
                &cfg,
                &client,
                &params,
                keypair,
                lane_subnet,
                lane_key,
                elfs,
                network_id,
                &mut persisted,
            )
            .await;
            (node, Some((settler, shutdown)))
        } else {
            let node = start_exec(
                &cfg,
                &client,
                &params,
                keypair,
                lane_subnet,
                lane_key,
                elfs,
                network_id,
                &mut persisted,
            )
            .await;
            (node, None)
        };

    // --- activity issuer (background, both modes) ---
    spawn_issuer(client.clone(), params.clone(), keypair, lane_subnet, lane_id, &cfg);

    let mode = if cfg.enable_settlements { "settlement" } else { "exec" };
    println!("== tn10-flow {mode} daemon: lane={lane_id} ==");
    println!(
        "watch RUST_LOG trace for vprogs_node_framework (blocks/reorgs/settlements) and \
         vprogs_zk_vm (decoded state)"
    );

    match settler {
        // Settlement mode: the settler is the daemon's reason to live. Awaiting its handle means a
        // panic inside it (rejected settlement, confirmation timeout) surfaces here and exits the
        // process non-zero, instead of being swallowed while the daemon parks looking healthy.
        Some((handle, shutdown)) => {
            // Ctrl-C / SIGTERM opens the settler's shutdown latch, so it tears down gracefully
            // (returning from its loop) instead of being killed mid-settlement.
            ctrlc::set_handler(move || shutdown.open()).expect("set signal handler");
            match handle.await {
                Ok(()) => log::info!("settler finished; shutting down"),
                Err(e) => {
                    log::error!("settler task terminated abnormally: {e}");
                    std::process::exit(1);
                }
            }
        }
        // Exec-only mode: nothing settles, so park until killed
        None => std::future::pending::<()>().await,
    }
}

/// Builds the execution-only node: reuse or dev-bootstrap a covenant, then a [`Node`] with no
/// proving. The bridge tracks the covenant's settlements; nothing here settles.
#[allow(clippy::too_many_arguments)]
async fn start_exec(
    cfg: &Config,
    client: &KaspaRpcClient,
    params: &Params,
    keypair: Keypair,
    lane_subnet: SubnetworkId,
    lane_key: kaspa_hashes::Hash,
    elfs: Elfs<'_>,
    network_id: NetworkId,
    persisted: &mut PersistedState,
) -> FlowNode {
    // --- resolve covenant: storage > env > dev-bootstrap ---
    let covenant_id = match persisted.covenant_hash().or(cfg.covenant_id_env) {
        Some(c) => {
            log::info!("reusing covenant {c}");
            c
        }
        None => {
            let wallet = Wallet::new(client, params, keypair);
            log::info!("bootstrapping dev covenant; issuer address {}", wallet.address());
            let (covenant, bootstrap_txid) =
                bootstrap_dev_covenant(&wallet, lane_key, COVENANT_VALUE).await;
            persisted.bootstrap_txid = Some(bootstrap_txid.to_string());
            log::info!("covenant {} bootstrapped (tx {})", covenant.covenant_id, bootstrap_txid);
            covenant.covenant_id
        }
    };
    // Persist the resolved id and any env-supplied bootstrap anchor so an env-supplied covenant
    // survives a restart without re-bootstrap (a catch-up node becomes a plain resume).
    persisted.covenant_id.get_or_insert_with(|| covenant_id.to_string());
    if let Some(txid) = cfg.bootstrap_txid {
        persisted.bootstrap_txid.get_or_insert_with(|| txid.to_string());
    }
    persisted.save(&cfg.data_dir);

    // Seed the bridge from the persisted deploy block if we have one (a covenant that has since
    // advanced still replays correctly forward from there), else from the env-supplied seed.
    let start_from = persisted.bootstrap_block().or(cfg.start_from);

    let store = daemon::Store::open(cfg.data_dir.join("db"));
    daemon::build_node(
        elfs,
        store,
        // Exec-only mode does not run the sync-progress reporter, so no DAA observer.
        bridge_params(cfg, network_id, lane_subnet, covenant_id, params, start_from, None),
    )
}

/// Builds the proving + settlement node: bootstrap a fresh covenant (dev-pins under
/// `RISC0_DEV_MODE`, real-pins otherwise) or, when `TN10_COVENANT_ID` is set, catch up to an
/// existing one (reconstructing its initial state from env instead of bootstrapping); then wire the
/// batch prover over the remote node and spawn the [`settler`] on the node's batch sink. A
/// fresh-bootstrap run starts at the empty SMT, so its data dir should be clean; a catch-up run
/// rebuilds state by replaying L1 forward from `TN10_START_FROM`.
///
/// Returns the node, the settler's [`JoinHandle`](tokio::task::JoinHandle) so `main` can await it
/// (the settler panics loudly on a rejected settlement or a confirmation timeout, and awaiting the
/// handle is what turns that into a visible process exit instead of a task that dies silently while
/// the daemon keeps parking), and the settler's shutdown latch so `main` can tear it down on a
/// signal.
#[allow(clippy::too_many_arguments)]
async fn start_settlement(
    cfg: &Config,
    client: &KaspaRpcClient,
    params: &Params,
    keypair: Keypair,
    lane_subnet: SubnetworkId,
    lane_key: kaspa_hashes::Hash,
    elfs: Elfs<'_>,
    network_id: NetworkId,
    persisted: &mut PersistedState,
) -> (FlowNode, tokio::task::JoinHandle<()>, AtomicAsyncLatch) {
    let backend = Backend::new(elfs.transaction, elfs.batch, elfs.aggregator, ProofType::Succinct);
    let wallet = Wallet::new(client, params, keypair);
    // Under `RISC0_DEV_MODE` the prover emits stub receipts the production `OpZkPrecompile` would
    // reject, so settle against the dev redeem (chain-anchored seq commit, no precompile); a real
    // (CUDA, non-dev) run settles against the production redeem. Same operating contract the tests
    // gate on.
    let dev = dev_mode_enabled();
    let mode = if dev { SettlementMode::Dev } else { SettlementMode::Production };
    // The node's selected tip right now: a real chain block at or just before any deploy block we
    // are about to mint (sink advances monotonically). Captured before bootstrap so it seeds the
    // bridge at or before the covenant, which the seed contract requires. For catch-up we keep the
    // operator-supplied `start_from` instead.
    let seed_block = client.get_block_dag_info().await.expect("get_block_dag_info").sink;
    // Catch-up: an env-supplied covenant id joins an existing covenant instead of bootstrapping a
    // fresh one, reconstructing the never-advanced state bootstrap produces. The bridge replays L1
    // from `start_from` and the settler self-heals from the on-chain last_settlement, so the
    // reconstruction is correct even when the covenant has already moved.
    //
    // `bootstrap_txid` is `None` here for a covenant that has already settled past its bootstrap:
    // the bootstrap UTXO is spent, so confirming it is pointless. The settler detects that at
    // startup and adopts the on-chain tip from `last_settlement` instead. The outpoint is a
    // placeholder in that case; only the first-settlement path needs the real bootstrap outpoint.
    let (covenant, bootstrap_txid) = if let Some(covenant_id) = cfg.covenant_id_env {
        // Same redeem builder bootstrap uses, so the reconstructed P2SH SPK matches the on-chain
        // covenant UTXO (asserted at the first settlement).
        let (_redeem, spk) = if dev {
            dev_bootstrap_redeem(&lane_key)
        } else {
            bootstrap_redeem(&backend, &lane_key)
        };
        // Real bootstrap outpoint if supplied, else a placeholder the settler replaces on adoption.
        let outpoint = match cfg.bootstrap_txid {
            Some(txid) => {
                log::info!("catching up to existing covenant {covenant_id} (bootstrap tx {txid})");
                TransactionOutpoint::new(txid, 0)
            }
            None => {
                log::info!(
                    "catching up to existing covenant {covenant_id} without a bootstrap txid; \
                     the settler adopts the on-chain tip if the bootstrap is already spent"
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
        (covenant, cfg.bootstrap_txid)
    } else if dev {
        log::info!(
            "settlement mode (dev): bootstrapping dev-pins covenant; issuer address {}",
            wallet.address()
        );
        let (covenant, txid) = bootstrap_dev_covenant(&wallet, lane_key, COVENANT_VALUE).await;
        (covenant, Some(txid))
    } else {
        log::info!(
            "settlement mode: bootstrapping real-pins covenant; issuer address {}",
            wallet.address()
        );
        let (covenant, txid) =
            bootstrap_real_covenant(&wallet, &backend, lane_key, COVENANT_VALUE).await;
        (covenant, Some(txid))
    };
    let covenant_id = covenant.covenant_id;
    // Persist the resolved id, the bootstrap anchor (when known), and the seed block so a restart
    // reuses them without env. Catch-up persists the operator-supplied `start_from`; a fresh deploy
    // persists the captured sink, both at or before the deploy block.
    persisted.covenant_id = Some(covenant_id.to_string());
    if let Some(txid) = bootstrap_txid {
        persisted.bootstrap_txid.get_or_insert_with(|| txid.to_string());
    }
    let seed_for_resume =
        if cfg.covenant_id_env.is_some() { cfg.start_from } else { Some(seed_block) };
    if let Some(seed) = seed_for_resume {
        persisted.bootstrap_block_hash.get_or_insert_with(|| seed.to_string());
    }
    persisted.save(&cfg.data_dir);
    log::info!("covenant {covenant_id} ready (bootstrap tx {bootstrap_txid:?})");

    // Seed the bridge from the persisted deploy block (resume) or the env seed (catch-up).
    let start_from = persisted.bootstrap_block().or(cfg.start_from);

    // The in-process aggregate prover publishes each proved bundle handle onto this queue; the
    // settlement worker pops from it and settles on chain.
    let queue: daemon::FlowSettlementQueue = daemon::FlowSettlementQueue::new();
    let store = daemon::Store::open(cfg.data_dir.join("db"));
    // The bridge replays from the pruning point and publishes its tip DAA here; a reporter task
    // polls it against the bootstrap's DAA to log how far the catch-up has progressed.
    let tip_daa = Arc::new(AtomicU64::new(0));
    let node = daemon::build_proving_node(
        elfs,
        store,
        bridge_params(
            cfg,
            network_id,
            lane_subnet,
            covenant_id,
            params,
            start_from,
            Some(tip_daa.clone()),
        ),
        ProvingParams {
            covenant_id,
            lane_key,
            client: client.clone(),
            sink: queue.clone(),
            bundle_size: 1..=usize::MAX,
        },
    );
    // Target the bridge replays toward: the node's virtual DAA now (the bootstrap was just sent, so
    // its block lands around here). Captured once; the reporter loop only reads the tip atomic.
    let target_daa =
        client.get_block_dag_info().await.expect("get_block_dag_info").virtual_daa_score;
    spawn_sync_reporter(tip_daa, target_daa);

    // The settlement worker drains bundle handles off the queue until `main` opens this latch on a
    // signal, or it hits a fatal error (a rejected settlement or a confirmation timeout). `main`
    // owns the returned handle and awaits it, so that failure is not swallowed.
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
            mode,
            submit_jitter: None,
            #[cfg(feature = "test-utils")]
            alternation: None,
        },
        covenant,
        shutdown.clone(),
    ));
    (node, settler, shutdown)
}

/// The bridge wiring for either mode, pointed at the remote node's lane + covenant. `start_from` is
/// the resolved seed block (persisted bootstrap block, else env), which the bridge seeds its
/// fresh-chain root at so a resume/catch-up replays L1 forward from the deploy.
fn bridge_params(
    cfg: &Config,
    network_id: NetworkId,
    lane_subnet: SubnetworkId,
    covenant_id: kaspa_hashes::Hash,
    params: &Params,
    start_from: Option<Hash>,
    tip_daa: Option<Arc<AtomicU64>>,
) -> BridgeParams {
    BridgeParams {
        url: cfg.wrpc_url.clone(),
        network_id,
        lane_subnet,
        covenant_id,
        finality_depth: params.finality_depth(),
        seed_depth: cfg.seed_depth,
        start_from,
        tip_daa,
    }
}

/// Spawns a background task that logs L1 catch-up progress while the bridge replays from the
/// pruning point toward the present. `target_daa` is the node's virtual DAA captured once right
/// after the bootstrap tx was sent (≈ the block the bootstrap lands in); the task polls the
/// bridge's published `tip_daa` every few seconds and logs a percentage until the tip reaches it.
/// The loop does no RPC; it only reads the atomic.
fn spawn_sync_reporter(tip_daa: Arc<AtomicU64>, target_daa: u64) {
    const POLL: Duration = Duration::from_secs(5);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(POLL);
        let mut start: Option<u64> = None;
        loop {
            ticker.tick().await;
            let current = tip_daa.load(Ordering::Relaxed);
            if current == 0 {
                continue; // bridge has not published a tip yet
            }
            if current >= target_daa {
                log::info!("L1 sync: caught up to bootstrap (daa {current} >= {target_daa})");
                return;
            }
            // Anchor the percentage at the first published tip (the pruning point we seed from).
            let start = *start.get_or_insert(current);
            let span = target_daa.saturating_sub(start).max(1);
            let pct =
                (current.saturating_sub(start) as f64 / span as f64 * 100.0).clamp(0.0, 100.0);
            log::info!(
                "L1 sync: {pct:.1}% (daa {current}/{target_daa}, {} behind)",
                target_daa.saturating_sub(current),
            );
        }
    });
}

/// Spawns the background activity issuer: every `activity_interval_ms`, build and submit one lane
/// transaction that writes the tracked resource, until `activity_count` is reached (0 = unbounded).
///
/// In settlement mode the issuer shares the funding key with the covenant bootstrap and the
/// settler; both pick the largest spendable UTXO, so a tight activity cadence can contend with
/// settlement funding. Keep the cadence modest (or fund a dedicated key) for settlement runs.
fn spawn_issuer(
    client: KaspaRpcClient,
    params: Params,
    keypair: Keypair,
    lane_subnet: SubnetworkId,
    lane_id: u32,
    cfg: &Config,
) {
    let interval = cfg.activity_interval_ms;
    let count = cfg.activity_count;
    let tracked = tracked_resource(lane_id);
    tokio::spawn(async move {
        let wallet = Wallet::new(&client, &params, keypair);
        let mut issued = 0u64;
        // Outpoints we have already spent this session. The node keeps reporting a mempool-spent
        // UTXO until it is mined (its change output stays invisible meanwhile), so without this we
        // would keep re-selecting the same largest UTXO and get rejected as a double spend. The
        // wallet prunes mined-away entries on each call.
        let mut in_flight = HashSet::new();
        loop {
            tokio::time::sleep(Duration::from_millis(interval)).await;
            if count != 0 && issued >= count {
                break;
            }
            let payload = encode_activity_payload(&[AccessMetadata::write(tracked)], &[1, 2, 3]);
            let (tx, outpoint) = match wallet
                .build_activity_excluding(payload, lane_subnet, TX_VERSION_TOCCATA, &mut in_flight)
                .await
            {
                Ok(Some(built)) => built,
                Ok(None) => {
                    log::warn!(
                        "issuer: no free spendable UTXOs for {} ({} in flight); fund it or wait \
                         for change to confirm",
                        wallet.address(),
                        in_flight.len(),
                    );
                    continue;
                }
                // A transient RPC failure (node restart, dropped connection): log and retry next
                // tick rather than taking the daemon down.
                Err(e) => {
                    log::warn!("issuer: spendable-utxo fetch failed (retrying next tick): {e}");
                    continue;
                }
            };
            match wallet.submit_transaction(&tx).await {
                Ok(id) => {
                    // Don't respend this UTXO until its change confirms.
                    in_flight.insert(outpoint);
                    log::info!("issued activity tx {id} on lane {lane_id}");
                    issued += 1;
                }
                Err(e) => {
                    // Mark it spent regardless: a double-spend rejection means it is already gone,
                    // and any other failure is more likely to recur on the same UTXO than a fresh
                    // one. The next iteration picks the next-largest free UTXO.
                    in_flight.insert(outpoint);
                    log::warn!("activity submit failed (retrying with another UTXO): {e}");
                }
            }
        }
        log::info!("issuer finished after {issued} activity txs");
    });
}

/// The single resource every activity transaction on `lane_id` writes to. Derived from the lane id
/// via [`ResourceIdExt::for_test`] so the counter is stable across restarts and matches the lane
/// key the prover and settlement use.
fn tracked_resource(lane_id: u32) -> ResourceId {
    ResourceId::for_test(lane_id as usize)
}

/// Connects a Borsh wRPC client to `url`, mirroring the bridge's own client construction.
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
