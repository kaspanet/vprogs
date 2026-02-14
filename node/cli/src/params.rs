use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};
use vprogs_node_framework::{NodeConfig, NodeVm};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{
    backend::{Store as BackendStore, Vm},
    execution_params::ExecutionParams,
    l1_bridge_params::L1BridgeParams,
    storage_params::StorageParams,
};

/// Top-level CLI arguments and configuration for the vprogs node.
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
    #[serde(skip)] // Not part of the config - only controls where to load the TOML from.
    pub config_file: PathBuf,
    /// Log level: error, warn, info, debug, trace. Overridden by RUST_LOG if set.
    #[arg(long, default_value = "info")]
    pub log_level: String,
    /// Bounded capacity of the node API request channel.
    #[arg(long, default_value_t = NodeConfig::<BackendStore, Vm>::default().api_channel_capacity)]
    pub api_channel_capacity: usize,
    /// Delete the data directory on startup before opening the store.
    #[arg(long)]
    #[serde(skip)] // One-shot action, not persisted config.
    pub reset: bool,
    #[command(flatten)]
    pub execution: ExecutionParams,
    #[command(flatten)]
    pub storage: StorageParams,
    #[command(flatten)]
    pub l1_bridge: L1BridgeParams,
}

impl NodeParams {
    /// Converts the top-level CLI params into a [`NodeConfig`], recursively converting each
    /// subsection and injecting the VM and store implementations.
    pub fn into_config<S: Store<StateSpace = StateSpace>, V: NodeVm>(
        self,
        vm: V,
        store: S,
    ) -> NodeConfig<S, V> {
        NodeConfig {
            api_channel_capacity: self.api_channel_capacity,
            execution_config: self.execution.into_config(vm),
            storage_config: self.storage.into_config(store),
            l1_bridge_config: self.l1_bridge.into_config(),
        }
    }
}
