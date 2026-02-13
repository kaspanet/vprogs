use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};
use vprogs_node_framework::{NodeConfig, NodeVm};
use vprogs_node_vm::VM;
use vprogs_state_space::StateSpace;
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
use vprogs_storage_types::Store;

use crate::{
    execution_params::ExecutionParams, l1_bridge_params::L1BridgeParams,
    storage_params::StorageParams,
};

/// vprogs L2 node for the Kaspa network.
#[derive(Parser, Serialize, Deserialize)]
#[command(
    name = "vprogs-node",
    version,
    before_help = "\
╻ ╻┏━┓┏━┓┏━┓┏━╸┏━┓   ┏┓╻┏━┓╺┳┓┏━╸
┃┏┛┣━┛┣┳┛┃ ┃┃╺┓┗━┓╺━╸┃┗┫┃ ┃ ┃┃┣╸
┗┛ ╹  ╹┗╸┗━┛┗━┛┗━┛   ╹ ╹┗━┛╺┻┛┗━╸",
    about = "vprogs L2 node for the Kaspa network.",
    long_about = "\
vprogs L2 node for the Kaspa network.

Runs a Layer 2 execution engine that processes transactions scheduled against \
Kaspa L1 blocks. Configuration is layered: built-in defaults → TOML config \
file → environment variables (VPROGS_*) → CLI arguments."
)]
pub struct NodeParams {
    /// Path to TOML config file.
    #[arg(long, default_value = "vprogs.toml")]
    #[serde(skip)]
    pub config_file: PathBuf,

    /// Log level: error, warn, info, debug, trace [default: info].
    /// Overridden by RUST_LOG if set.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,

    /// Bounded capacity of the node API request channel [default: 64].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_channel_capacity: Option<usize>,

    /// Delete the data directory on startup before opening the store.
    #[arg(long)]
    #[serde(skip)]
    pub reset: bool,

    #[command(flatten)]
    pub execution: ExecutionParams,

    #[command(flatten)]
    pub storage: StorageParams,

    #[command(flatten)]
    pub l1_bridge: L1BridgeParams,
}

impl NodeParams {
    pub fn to_config<S: Store<StateSpace = StateSpace>, V: NodeVm>(
        self,
        vm: V,
        store: S,
    ) -> NodeConfig<S, V> {
        NodeConfig {
            api_channel_capacity: self.api_channel_capacity.expect("api_channel_capacity"),
            execution_config: self.execution.to_config(vm),
            storage_config: self.storage.to_config(store),
            l1_bridge_config: self.l1_bridge.to_config(),
        }
    }
}

impl Default for NodeParams {
    fn default() -> Self {
        let default_config = NodeConfig::<RocksDbStore<DefaultConfig>, VM>::default();
        Self {
            config_file: PathBuf::from("vprogs.toml"),
            log_level: Some("info".to_string()),
            api_channel_capacity: Some(default_config.api_channel_capacity),
            reset: false,
            execution: ExecutionParams::default(),
            storage: StorageParams::default(),
            l1_bridge: L1BridgeParams::default(),
        }
    }
}
