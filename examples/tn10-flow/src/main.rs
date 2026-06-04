//! tn10-flow: a proof-of-concept that runs the L2 flow against a remote testnet-10 fork node.
//!
//! One env-driven process: bootstrap a covenant (or reuse a stored one), periodically issue
//! activity transactions on a lane, and run an execution-only daemon that follows the chain via the
//! existing [`L1Bridge`], executes the lane's transactions through the same VM the settlement-l1
//! tests use, and prints the decoded state counter, reorgs, and settlements.
//!
//! Prover mode is the next feature and will live behind the `cuda` feature; nothing here needs it.
//!
//! Required env: `TN10_WRPC_URL`, `TN10_PRIVATE_KEY`. See `config.rs` for the full surface.

mod bootstrap;
mod config;
mod daemon;
mod params;
mod persistence;
mod state_read;

use std::time::Duration;

use kaspa_addresses::Prefix;
use kaspa_consensus_core::{
    constants::TX_VERSION_TOCCATA,
    network::{NetworkId, NetworkType},
    subnets::SubnetworkId,
};
use kaspa_seq_commit::hashing::lane_key;
use kaspa_wrpc_client::prelude::*;
use secp256k1::Keypair;
use vprogs_core_types::AccessMetadata;
use vprogs_l1_wallet::{Wallet, encode_activity_payload};

use crate::{
    bootstrap::bootstrap_covenant, config::Config, daemon::BridgeParams, params::tn10_params,
    persistence::PersistedState, state_read::tracked_resource,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    kaspa_core::log::try_init_logger("info,tn10_flow=info");

    let cfg = Config::from_env();
    let params = tn10_params();
    let network_id = NetworkId::with_suffix(NetworkType::Testnet, 10);

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

    // --- resolve covenant: storage > env > bootstrap ---
    let covenant_id = match persisted.covenant_hash().or(cfg.covenant_id_env) {
        Some(c) => {
            log::info!("reusing covenant {c}");
            c
        }
        None => {
            let wallet = Wallet::new(&client, &params, keypair, Prefix::Testnet);
            log::info!("bootstrapping covenant; issuer address {}", wallet.address());
            let booted = bootstrap_covenant(&wallet, &lane_key).await;
            persisted.covenant_id = Some(booted.covenant_id.to_string());
            persisted.bootstrap_txid = Some(booted.bootstrap_txid.to_string());
            log::info!(
                "covenant {} bootstrapped (tx {})",
                booted.covenant_id,
                booted.bootstrap_txid
            );
            booted.covenant_id
        }
    };
    persisted.save(&cfg.data_dir);

    // --- VM + scheduler (execution only) ---
    let tx_elf = std::fs::read(&cfg.tx_elf_path)
        .unwrap_or_else(|e| panic!("read tx-processor ELF {:?}: {e}", cfg.tx_elf_path));
    let batch_elf = std::fs::read(&cfg.batch_elf_path)
        .unwrap_or_else(|e| panic!("read batch-processor ELF {:?}: {e}", cfg.batch_elf_path));
    let store = daemon::Store::open(cfg.data_dir.join("db"));
    let scheduler = daemon::build_scheduler(&tx_elf, &batch_elf, store.clone());

    // --- activity issuer (background) ---
    {
        let client = client.clone();
        let params = params.clone();
        let interval = cfg.activity_interval_ms;
        let count = cfg.activity_count;
        let tracked = tracked_resource(lane_id);
        tokio::spawn(async move {
            let wallet = Wallet::new(&client, &params, keypair, Prefix::Testnet);
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
                let payload =
                    encode_activity_payload(&[AccessMetadata::write(tracked)], &[1, 2, 3]);
                let built = wallet
                    .build_subnet_payload_transactions(
                        vec![payload],
                        lane_subnet,
                        TX_VERSION_TOCCATA,
                    )
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

    // --- run the execution daemon (forever) ---
    println!("== tn10-flow exec daemon: lane={lane_id} covenant={covenant_id} ==");
    let bridge_params = BridgeParams {
        url: cfg.wrpc_url,
        network_id,
        lane_subnet,
        covenant_id,
        finality_depth: params.finality_depth(),
    };
    daemon::run(scheduler, store, bridge_params, tracked_resource(lane_id)).await;
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
