//! tn10-flow: a proof-of-concept that runs the L2 flow against a remote testnet-10 fork node.
//!
//! It is now a thin driver over the [`vprogs_runner`] engine: the runner owns fetch → execute →
//! (optional) prove → settle and the fresh/resume/catch-up start modes; this example adds only the
//! background **activity issuer** that periodically writes a tracked resource on a lane, exercising
//! the flow. It runs in one of two modes (`TN10_SETTLE=1` selects proving + settlement); under
//! `RISC0_DEV_MODE` the prover emits stub receipts and settles against the dev redeem, so the whole
//! flow runs without a GPU.
//!
//! Required env: `TN10_WRPC_URL`, `TN10_PRIVATE_KEY`. See `config.rs` for the full surface.

mod config;

use std::{collections::HashSet, time::Duration};

use kaspa_consensus_core::{
    config::params::Params, constants::TX_VERSION_TOCCATA, subnets::SubnetworkId,
};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_wallet::{Wallet, encode_activity_payload};
use vprogs_runner::{Elfs, connect_wrpc, start_runner};
use vprogs_zk_backend_risc0_test_suite::{
    batch_aggregator_elf, batch_processor_elf, transaction_processor_elf,
};

use crate::config::Config;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    kaspa_core::log::try_init_logger(
        "info,tn10_flow=info,vprogs_node_framework=trace,vprogs_zk_vm=trace,risc0_zkvm=warn",
    );

    let cfg = Config::from_env();
    let network_id = cfg.runner.network_id;

    // Network params, used off-chain only for mass calculation and lane-key derivation; never pushed
    // to the node (the remote fork node runs its own params, with the covenant forks active).
    let params = Params::from(network_id);
    let client = connect_wrpc(&cfg.runner.wrpc_url, network_id).await;
    log::info!("connected to {}", cfg.runner.wrpc_url);

    let keypair = Keypair::from_secret_key(secp256k1::SECP256K1, &cfg.runner.private_key);
    // This example runs the trivial transaction-processor guest; a program-agnostic run uses `vprun`
    // with `--program-elf`.
    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };

    // The runner builds and starts the node (kept alive until we return) and, in settlement mode,
    // bootstraps the covenant and spawns the settler.
    let handles = start_runner(&cfg.runner, &client, &params, elfs)
        .await
        .unwrap_or_else(|e| panic!("runner start failed: {e}"));

    // The activity issuer is this example's reason to exist: it produces the lane transactions the
    // runner then executes/proves. The generic runner never issues any.
    spawn_issuer(
        client.clone(),
        params.clone(),
        keypair,
        handles.lane_subnet,
        handles.lane_id,
        cfg.activity_interval_ms,
        cfg.activity_count,
    );

    let mode = if cfg.runner.prove { "settlement" } else { "exec" };
    println!("== tn10-flow {mode} daemon: lane={} ==", handles.lane_id);
    println!(
        "watch RUST_LOG trace for vprogs_node_framework (blocks/reorgs/settlements) and \
         vprogs_zk_vm (decoded state)"
    );

    // Keep the node alive for the lifetime of the process.
    let _node = handles.node;
    match handles.settler {
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
        // Exec-only mode: nothing settles, so park until killed.
        None => std::future::pending::<()>().await,
    }
}

/// Spawns the background activity issuer: every `interval_ms`, build and submit one lane transaction
/// that writes the tracked resource, until `count` is reached (0 = unbounded).
///
/// In settlement mode the issuer shares the funding key with the covenant bootstrap and the settler;
/// both pick the largest spendable UTXO, so a tight activity cadence can contend with settlement
/// funding. Keep the cadence modest (or fund a dedicated key) for settlement runs.
#[allow(clippy::too_many_arguments)]
fn spawn_issuer(
    client: KaspaRpcClient,
    params: Params,
    keypair: Keypair,
    lane_subnet: SubnetworkId,
    lane_id: u32,
    interval_ms: u64,
    count: u64,
) {
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
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
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
/// via [`ResourceIdExt::for_test`] so the counter is stable across restarts and matches the lane key
/// the prover and settlement use.
fn tracked_resource(lane_id: u32) -> ResourceId {
    ResourceId::for_test(lane_id as usize)
}
