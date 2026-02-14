use std::time::Duration;

use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_node_l1_bridge::L1BridgeConfig;

use crate::extensions::ConnectStrategyExt;

/// CLI arguments for the L1 bridge (Kaspa node connection and reorg filtering).
#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "L1 Bridge")]
pub struct L1BridgeParams {
    /// WebSocket URL for the Kaspa L1 node (e.g. ws://localhost:17110).
    /// Omit to use the public resolver.
    #[arg(long = "l1-bridge-url")]
    pub url: Option<String>,
    /// Target network: mainnet, testnet-10, testnet-11, devnet, simnet.
    #[arg(long = "l1-bridge-network-id", default_value_t = L1BridgeConfig::default().network_id.to_string())]
    pub network_id: String,
    /// L1 connection timeout in milliseconds.
    #[arg(long = "l1-bridge-connect-timeout-ms", default_value_t = L1BridgeConfig::default().connect_timeout_ms)]
    pub connect_timeout_ms: u64,
    /// Connection strategy: retry (block until connected) or fallback (fail fast).
    #[arg(long = "l1-bridge-connect-strategy", default_value_t = L1BridgeConfig::default().connect_strategy.to_string())]
    pub connect_strategy: String,
    /// Reorg filter half-life in seconds. Observed reorg depths accumulate into a threshold
    /// that halves every half-life. Set to 0 to disable.
    #[arg(long = "l1-bridge-filter-half-life-secs", default_value_t = L1BridgeConfig::default().filter_half_life.as_secs())]
    pub filter_half_life_secs: u64,
}

impl L1BridgeParams {
    /// Converts CLI params into an [`L1BridgeConfig`], parsing string fields into their typed
    /// representations. Root and tip are left unset - they are populated from the scheduler's
    /// persisted state at startup.
    pub fn into_config(self) -> L1BridgeConfig {
        L1BridgeConfig {
            url: self.url,
            network_id: self.network_id.parse().expect("invalid network id"),
            connect_timeout_ms: self.connect_timeout_ms,
            connect_strategy: self.connect_strategy.parse().expect("invalid connect strategy"),
            filter_half_life: Duration::from_secs(self.filter_half_life_secs),
            root: None,
            tip: None,
        }
    }
}
