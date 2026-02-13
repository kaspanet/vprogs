use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

use crate::{
    execution_params::ExecutionParams, l1_bridge_params::L1BridgeParams,
    storage_params::StorageParams,
};

/// vprogs L2 node for the Kaspa network.
#[derive(Parser, Deserialize)]
#[command(name = "vprogs-node", version)]
pub struct NodeParams {
    /// Path to TOML config file.
    #[arg(long, default_value = "vprogs.toml")]
    #[serde(skip)]
    pub config_file: PathBuf,

    /// Log level: error, warn, info, debug, trace. Overridden by RUST_LOG if set.
    #[arg(long, default_value = "info")]
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Bounded capacity of the node API request channel.
    #[arg(long, default_value_t = 64)]
    #[serde(default = "default_api_channel_capacity")]
    pub api_channel_capacity: usize,

    #[command(flatten)]
    pub execution: ExecutionParams,

    #[command(flatten)]
    pub storage: StorageParams,

    #[command(flatten)]
    pub l1_bridge: L1BridgeParams,
}

impl NodeParams {
    /// Merges with file-based params. `self` (CLI) takes precedence for `Option` fields;
    /// file values fill in anything the CLI left unset.
    pub fn merge(mut self, file: NodeParams) -> Self {
        self.execution.workers = self.execution.workers.or(file.execution.workers);
        self.l1_bridge.kaspa_url = self.l1_bridge.kaspa_url.take().or(file.l1_bridge.kaspa_url);
        self
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_api_channel_capacity() -> usize {
    64
}
