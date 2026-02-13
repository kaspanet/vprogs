mod execution_params;
mod l1_bridge_params;
mod params;
mod storage_params;

use std::{fs, sync::mpsc, time::Duration};

use clap::Parser;
use log::info;
use vprogs_node_framework::{Node, NodeConfig};
use vprogs_node_l1_bridge::{ConnectStrategy, L1BridgeConfig, NetworkId};
use vprogs_node_vm::VM;
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_storage_manager::{ReadConfig, StorageConfig, WriteConfig};
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};

use crate::params::NodeParams;

fn main() {
    let mut params = NodeParams::parse();

    // Initialize logging. RUST_LOG overrides --log-level when set.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&params.log_level))
        .init();

    // Merge config file (if it exists) — CLI args take precedence.
    if params.config_file.exists() {
        info!("loading config from {}", params.config_file.display());
        let content = fs::read_to_string(&params.config_file).expect("failed to read config file");
        let file_params: NodeParams =
            toml::from_str(&content).expect("failed to parse config file");
        params = params.merge(file_params);
    }

    // Build runtime objects.
    let store = RocksDbStore::<DefaultConfig>::open(&params.storage.data_dir);
    let vm = VM;

    let network_id: NetworkId = params.l1_bridge.network.parse().expect("invalid network id");

    // Assemble configs from params.
    let execution = {
        let mut config = ExecutionConfig::new().with_vm(vm);
        if let Some(workers) = params.execution.workers {
            config = config.with_worker_count(workers);
        }
        config
    };

    let storage = StorageConfig::new()
        .with_store(store)
        .with_read_config(
            ReadConfig::new()
                .with_max_readers(params.storage.max_readers)
                .with_buffer_depth_per_reader(params.storage.buffer_depth_per_reader),
        )
        .with_write_config(
            WriteConfig::new()
                .with_max_batch_size(params.storage.max_batch_size)
                .with_max_batch_duration(Duration::from_millis(
                    params.storage.max_batch_duration_ms,
                )),
        );

    let connect_strategy: ConnectStrategy = params
        .l1_bridge
        .connect_strategy
        .parse()
        .expect("invalid connect strategy (expected 'retry' or 'fallback')");

    let l1_bridge = {
        let mut config = L1BridgeConfig::default()
            .with_network_id(network_id)
            .with_connect_timeout(params.l1_bridge.connect_timeout_ms)
            .with_connect_strategy(connect_strategy)
            .with_reorg_filter_halving_period(Duration::from_secs(
                params.l1_bridge.reorg_filter_halving_period_secs,
            ));
        if let Some(url) = params.l1_bridge.kaspa_url {
            config = config.with_url(url);
        }
        config
    };

    let config = NodeConfig::new(execution, storage, l1_bridge)
        .with_api_channel_capacity(params.api_channel_capacity);

    // Start the node.
    info!("starting vprogs node on {network_id}");
    let node = Node::new(config);

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
