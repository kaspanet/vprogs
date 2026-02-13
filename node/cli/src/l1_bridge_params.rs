use clap::Args;
use serde::Deserialize;

#[derive(Args, Deserialize)]
#[command(next_help_heading = "L1 Bridge")]
pub struct L1BridgeParams {
    /// WebSocket URL for the Kaspa L1 node (e.g. ws://localhost:17110).
    /// Omit to use the public resolver.
    #[arg(long)]
    pub kaspa_url: Option<String>,

    /// Target network: mainnet, testnet-10, testnet-11, devnet, simnet.
    #[arg(long, default_value = "mainnet")]
    #[serde(default = "default_network")]
    pub network: String,

    /// L1 connection timeout in milliseconds.
    #[arg(long, default_value_t = 10_000)]
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,

    /// Connection strategy: retry (block until connected) or fallback (fail fast).
    #[arg(long, default_value = "retry")]
    #[serde(default = "default_connect_strategy")]
    pub connect_strategy: String,

    /// Reorg filter halving period in seconds. Observed reorg depths accumulate into a
    /// threshold that halves every period. Set to 0 to disable.
    #[arg(long, default_value_t = 3600)]
    #[serde(default = "default_reorg_filter_halving_period_secs")]
    pub reorg_filter_halving_period_secs: u64,
}

fn default_network() -> String {
    "mainnet".to_string()
}

fn default_connect_timeout_ms() -> u64 {
    10_000
}

fn default_connect_strategy() -> String {
    "retry".to_string()
}

fn default_reorg_filter_halving_period_secs() -> u64 {
    3600
}
