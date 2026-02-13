mod execution_params;
mod l1_bridge_params;
mod params;
mod storage_params;

use std::sync::mpsc;

use clap::Parser;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use log::info;
use vprogs_node_framework::{Node, NodeConfig};
use vprogs_node_l1_bridge::L1BridgeConfig;
use vprogs_node_vm::VM;
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};

use crate::params::NodeParams;

fn main() {
    let params = NodeParams::parse();

    // Initialize logging early. RUST_LOG overrides --log-level when set.
    let log_level = params.log_level.as_deref().unwrap_or("info");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();

    // Layered config: defaults → TOML file → env vars → CLI args.
    let params: NodeParams = Figment::from(Serialized::defaults(NodeParams::default()))
        .merge(Toml::file(&params.config_file))
        .merge(Env::prefixed("VPROGS_").split("__"))
        .merge(Serialized::globals(&params))
        .extract()
        .expect("invalid configuration");

    info!("loaded config from: defaults → {} → env → cli", params.config_file.display());

    // Build runtime objects.
    let data_dir = params.storage.data_dir.clone().expect("data_dir");
    let store = RocksDbStore::<DefaultConfig>::open(&data_dir);

    // Start the node.
    info!("starting vprogs node");
    let node = Node::new(
        NodeConfig::default()
            .with_api_channel_capacity(params.api_channel_capacity.expect("api_channel_capacity"))
            .with_execution_config(ExecutionConfig::from(params.execution).with_vm(VM))
            .with_storage_config(StorageConfig::from(params.storage).with_store(store))
            .with_l1_bridge_config(L1BridgeConfig::from(params.l1_bridge)),
    );

    // Wait for shutdown signal (Ctrl-C / SIGTERM).
    let (tx, rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .expect("failed to set signal handler");
    rx.recv().ok();

    // Graceful shutdown.
    info!("shutting down");
    node.shutdown();
}
