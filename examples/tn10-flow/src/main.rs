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
//! - **Proving + settlement** (`TN10_SETTLE=1`): the node drives the real batch prover and forwards
//!   each scheduled batch to the [`settler`], which proves every bundle and settles it on chain
//!   with a production [`Settlement::build`](vprogs_zk_backend_risc0_covenant::Settlement) — the
//!   same end-to-end shape the `vprogs-sim` `l2_flow_real_proof_settlement_chain` test exercises
//!   against a simulated DAG, here against a live node. Real proofs need a GPU (the `cuda` feature,
//!   without `RISC0_DEV_MODE`); the wiring compiles and runs with stub proofs otherwise, but the
//!   on-chain `OpZkPrecompile` only accepts real receipts.
//!
//! The covenant bootstrap, fork toggle, scheduler construction, and ELF loading are reused from the
//! settlement test-suite (`vprogs_zk_backend_risc0_test_suite`); the settlement build/chain logic
//! mirrors `sim::driver::settle_real`. This binary adds the remote-connection, persistence, issuer,
//! and node/settler glue.
//!
//! Required env: `TN10_WRPC_URL`, `TN10_PRIVATE_KEY`. See `config.rs` for the full surface.

mod config;
mod daemon;
mod persistence;
mod settler;

use std::time::Duration;

use kaspa_consensus_core::{
    config::params::Params,
    constants::TX_VERSION_TOCCATA,
    network::{NetworkId, NetworkType},
    subnets::SubnetworkId,
};
use kaspa_seq_commit::hashing::lane_key;
use kaspa_wrpc_client::prelude::*;
use secp256k1::Keypair;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_wallet::{Wallet, encode_activity_payload};
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_test_suite::{
    batch_processor_elf, bootstrap_dev_covenant, force_covenant_forks, transaction_processor_elf,
};

use crate::{
    config::Config,
    daemon::{BridgeParams, FlowNode, ProvingParams, RemoteLaneSource},
    persistence::PersistedState,
    settler::SettlerConfig,
};

/// Value locked in the covenant UTXO at bootstrap (1 TKAS), matching the e2e tests.
const COVENANT_VALUE: u64 = 100_000_000;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // The framework worker traces per-block processing (`vprogs_node_framework`) and the zk Vm
    // traces post-execution L2 state (`vprogs_zk_vm`); enable both at trace so the POC surfaces the
    // found txs / reorgs / settlements / decoded counter without a hand-rolled loop.
    kaspa_core::log::try_init_logger(
        "info,tn10_flow=info,vprogs_node_framework=trace,vprogs_zk_vm=trace",
    );

    let cfg = Config::from_env();
    let network_id = NetworkId::with_suffix(NetworkType::Testnet, 10);

    // testnet-10 params with the covenant forks forced active (the user's fork node forces them
    // on); used off-chain for mass calculation and lane-key derivation, never pushed to the
    // node.
    let mut params = Params::from(network_id);
    force_covenant_forks(&mut params);

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

    // Build the node (kept alive until we park; dropping it shuts the flow down). Settlement mode
    // additionally bootstraps a real-pins covenant and spawns the settler, which drains the batch
    // sink the node forwards to it.
    let _node: FlowNode = if cfg.enable_settlements {
        start_settlement(
            &cfg,
            &client,
            &params,
            keypair,
            lane_subnet,
            lane_key,
            &tx_elf,
            &batch_elf,
            network_id,
            &mut persisted,
        )
        .await
    } else {
        start_exec(
            &cfg,
            &client,
            &params,
            keypair,
            lane_subnet,
            lane_key,
            &tx_elf,
            &batch_elf,
            network_id,
            &mut persisted,
        )
        .await
    };

    // --- activity issuer (background, both modes) ---
    spawn_issuer(client.clone(), params.clone(), keypair, lane_subnet, lane_id, &cfg);

    let mode = if cfg.enable_settlements { "settlement" } else { "exec" };
    println!("== tn10-flow {mode} daemon: lane={lane_id} ==");
    println!(
        "watch RUST_LOG trace for vprogs_node_framework (blocks/reorgs/settlements) and \
         vprogs_zk_vm (decoded state)"
    );
    std::future::pending::<()>().await;
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
    tx_elf: &[u8],
    batch_elf: &[u8],
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
            let booted = bootstrap_dev_covenant(&wallet, &lane_key, COVENANT_VALUE).await;
            persisted.bootstrap_txid = Some(booted.bootstrap_txid.to_string());
            log::info!(
                "covenant {} bootstrapped (tx {})",
                booted.covenant_id,
                booted.bootstrap_txid
            );
            booted.covenant_id
        }
    };
    // Persist the resolved id so an env-supplied covenant survives a restart without re-bootstrap.
    persisted.covenant_id.get_or_insert_with(|| covenant_id.to_string());
    persisted.save(&cfg.data_dir);

    let store = daemon::Store::open(cfg.data_dir.join("db"));
    daemon::build_node(
        tx_elf,
        batch_elf,
        store,
        bridge_params(cfg, network_id, lane_subnet, covenant_id, params),
    )
}

/// Builds the proving + settlement node: bootstrap a fresh real-pins covenant, wire the batch
/// prover over the remote node, and spawn the [`settler`] on the node's batch sink. Always
/// bootstraps fresh (the prover's store starts at the empty SMT, which the bootstrap's initial
/// state matches), so the data dir should be clean for a settlement run.
#[allow(clippy::too_many_arguments)]
async fn start_settlement(
    cfg: &Config,
    client: &KaspaRpcClient,
    params: &Params,
    keypair: Keypair,
    lane_subnet: SubnetworkId,
    lane_key: kaspa_hashes::Hash,
    tx_elf: &[u8],
    batch_elf: &[u8],
    network_id: NetworkId,
    persisted: &mut PersistedState,
) -> FlowNode {
    let backend = Backend::new(tx_elf, batch_elf, ProofType::Succinct);
    let wallet = Wallet::new(client, params, keypair);
    log::info!(
        "settlement mode: bootstrapping real-pins covenant; issuer address {}",
        wallet.address()
    );
    let (covenant, bootstrap_txid) =
        settler::bootstrap_real_covenant(&wallet, &backend, lane_key, COVENANT_VALUE).await;
    let covenant_id = covenant.covenant_id;
    persisted.covenant_id = Some(covenant_id.to_string());
    persisted.bootstrap_txid = Some(bootstrap_txid.to_string());
    persisted.save(&cfg.data_dir);
    log::info!("covenant {covenant_id} bootstrapped (tx {bootstrap_txid})");

    let (sink_tx, sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let store = daemon::Store::open(cfg.data_dir.join("db"));
    let node = daemon::build_proving_node(
        tx_elf,
        batch_elf,
        store,
        bridge_params(cfg, network_id, lane_subnet, covenant_id, params),
        ProvingParams {
            lane_source: RemoteLaneSource::new(client.clone()),
            covenant_id,
            lane_key,
            bundle_size: cfg.bundle_size,
            sink: sink_tx,
        },
    );

    // The settler runs until the node drops (closing the sink). A detached task: its JoinHandle is
    // dropped, which does not cancel it.
    tokio::spawn(settler::run(
        sink_rx,
        SettlerConfig {
            client: client.clone(),
            params: params.clone(),
            keypair,
            lane_key,
            bundle_size: cfg.bundle_size,
            tx_elf: tx_elf.to_vec(),
            batch_elf: batch_elf.to_vec(),
        },
        covenant,
    ));
    node
}

/// The bridge wiring for either mode, pointed at the remote node's lane + covenant.
fn bridge_params(
    cfg: &Config,
    network_id: NetworkId,
    lane_subnet: SubnetworkId,
    covenant_id: kaspa_hashes::Hash,
    params: &Params,
) -> BridgeParams {
    BridgeParams {
        url: cfg.wrpc_url.clone(),
        network_id,
        lane_subnet,
        covenant_id,
        finality_depth: params.finality_depth(),
    }
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
        loop {
            tokio::time::sleep(Duration::from_millis(interval)).await;
            if count != 0 && issued >= count {
                break;
            }
            if wallet.spendable_utxo_count().await == 0 {
                log::warn!(
                    "issuer: no spendable UTXOs for {}; fund it to issue activity",
                    wallet.address()
                );
                continue;
            }
            let payload = encode_activity_payload(&[AccessMetadata::write(tracked)], &[1, 2, 3]);
            let built = wallet
                .build_subnet_payload_transactions(vec![payload], lane_subnet, TX_VERSION_TOCCATA)
                .await;
            for tx in &built {
                match wallet.submit_transaction(tx).await {
                    Ok(id) => log::info!("issued activity tx {id} on lane {lane_id}"),
                    Err(e) => log::warn!("activity submit failed: {e}"),
                }
            }
            issued += 1;
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
