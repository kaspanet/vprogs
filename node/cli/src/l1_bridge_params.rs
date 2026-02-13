use std::time::Duration;

use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_node_l1_bridge::{ConnectStrategy, L1BridgeConfig};

#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "L1 Bridge")]
pub struct L1BridgeParams {
    /// WebSocket URL for the Kaspa L1 node (e.g. ws://localhost:17110).
    /// Omit to use the public resolver.
    #[arg(long = "l1-bridge-url")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Target network: mainnet, testnet-10, testnet-11, devnet, simnet [default: mainnet].
    #[arg(long = "l1-bridge-network-id")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_id: Option<String>,

    /// L1 connection timeout in milliseconds [default: 10000].
    #[arg(long = "l1-bridge-connect-timeout-ms")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout_ms: Option<u64>,

    /// Connection strategy: retry (block until connected) or fallback (fail fast) [default:
    /// retry].
    #[arg(long = "l1-bridge-connect-strategy")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_strategy: Option<String>,

    /// Reorg filter halving period in seconds. Observed reorg depths accumulate into a
    /// threshold that halves every period. Set to 0 to disable [default: 0 (disabled)].
    #[arg(long = "l1-bridge-reorg-filter-halving-period-secs")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reorg_filter_halving_period_secs: Option<u64>,
}

impl L1BridgeParams {
    pub fn to_config(self) -> L1BridgeConfig {
        L1BridgeConfig {
            url: self.url,
            network_id: self.network_id.expect("network_id").parse().expect("invalid network id"),
            connect_timeout_ms: self.connect_timeout_ms.expect("connect_timeout_ms"),
            connect_strategy: self
                .connect_strategy
                .expect("connect_strategy")
                .parse()
                .expect("invalid connect strategy"),
            reorg_filter_halving_period: Duration::from_secs(
                self.reorg_filter_halving_period_secs.expect("reorg_filter_halving_period_secs"),
            ),
            root: None,
            tip: None,
        }
    }
}

impl Default for L1BridgeParams {
    fn default() -> Self {
        let default_config = L1BridgeConfig::default();
        Self {
            url: default_config.url,
            network_id: Some(default_config.network_id.to_string()),
            connect_timeout_ms: Some(default_config.connect_timeout_ms),
            connect_strategy: Some(
                match default_config.connect_strategy {
                    ConnectStrategy::Retry => "retry",
                    ConnectStrategy::Fallback => "fallback",
                }
                .to_string(),
            ),
            reorg_filter_halving_period_secs: Some(
                default_config.reorg_filter_halving_period.as_secs(),
            ),
        }
    }
}
