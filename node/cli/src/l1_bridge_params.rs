use std::time::Duration;

use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_node_l1_bridge::{ConnectStrategy, L1BridgeConfig};

#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "L1 Bridge")]
pub struct L1BridgeParams {
    /// WebSocket URL for the Kaspa L1 node (e.g. ws://localhost:17110).
    /// Omit to use the public resolver.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kaspa_url: Option<String>,

    /// Target network: mainnet, testnet-10, testnet-11, devnet, simnet [default: mainnet].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,

    /// L1 connection timeout in milliseconds [default: 10000].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout_ms: Option<u64>,

    /// Connection strategy: retry (block until connected) or fallback (fail fast) [default:
    /// retry].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_strategy: Option<String>,

    /// Reorg filter halving period in seconds. Observed reorg depths accumulate into a
    /// threshold that halves every period. Set to 0 to disable [default: 3600].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reorg_filter_halving_period_secs: Option<u64>,
}

impl Default for L1BridgeParams {
    fn default() -> Self {
        let default_config = L1BridgeConfig::default();
        Self {
            kaspa_url: default_config.url,
            network: Some(default_config.network_id.to_string()),
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

impl From<L1BridgeParams> for L1BridgeConfig {
    fn from(params: L1BridgeParams) -> Self {
        Self::default()
            .with_url(params.kaspa_url)
            .with_network_id(params.network.expect("network").parse().expect("invalid network id"))
            .with_connect_timeout(params.connect_timeout_ms.expect("connect_timeout_ms"))
            .with_connect_strategy(
                params
                    .connect_strategy
                    .expect("connect_strategy")
                    .parse()
                    .expect("invalid connect strategy"),
            )
            .with_reorg_filter_halving_period(Duration::from_secs(
                params.reorg_filter_halving_period_secs.expect("reorg_filter_halving_period_secs"),
            ))
    }
}
