use std::time::Duration;

use vprogs_core_types::Checkpoint;

use crate::{ChainBlockMetadata, ConnectStrategy, NetworkId, NetworkType};

/// Configuration for the L1 bridge.
#[derive(Clone, Debug)]
pub struct L1BridgeConfig {
    /// WebSocket URL (e.g. `ws://localhost:17110`), or `None` to use the public resolver.
    pub url: Option<String>,
    /// Target network (mainnet, testnet, devnet, simnet).
    pub network_id: NetworkId,
    /// Connection timeout in milliseconds.
    pub connect_timeout_ms: u64,
    /// Reconnection strategy.
    pub connect_strategy: ConnectStrategy,
    /// Finalization boundary, or `None` to start from the L1 pruning point. When set together with
    /// `tip`, the bridge backfills the chain between them on startup.
    pub root: Option<Checkpoint<ChainBlockMetadata>>,
    /// Last known tip, or `None` to start from the L1 pruning point.
    pub tip: Option<Checkpoint<ChainBlockMetadata>>,
    /// Reorg filter half-life. Observed reorg depths accumulate into a threshold that halves
    /// every half-life until it decays to zero. Set to `Duration::ZERO` to disable.
    pub filter_half_life: Duration,
}

impl Default for L1BridgeConfig {
    /// Defaults to mainnet with a 10-second timeout, automatic reconnection, and no resume state.
    fn default() -> Self {
        Self {
            url: None, // Use the public resolver.
            network_id: NetworkId::new(NetworkType::Mainnet),
            connect_timeout_ms: 10_000,
            connect_strategy: ConnectStrategy::Retry, // Reconnect automatically on disconnect.
            root: None,                               // Start from the L1 pruning point.
            tip: None,
            filter_half_life: Duration::from_secs(3600), // 1 hour.
        }
    }
}

impl L1BridgeConfig {
    /// Sets the WebSocket URL for the L1 node.
    pub fn with_url(mut self, url: Option<impl Into<String>>) -> Self {
        self.url = url.map(Into::into);
        self
    }

    /// Sets the target network by full identifier.
    pub fn with_network_id(mut self, network_id: NetworkId) -> Self {
        self.network_id = network_id;
        self
    }

    /// Sets the target network by type (uses default suffix).
    pub fn with_network_type(mut self, network_type: NetworkType) -> Self {
        self.network_id = NetworkId::new(network_type);
        self
    }

    /// Sets the connection timeout in milliseconds.
    pub fn with_connect_timeout(mut self, timeout_ms: u64) -> Self {
        self.connect_timeout_ms = timeout_ms;
        self
    }

    /// Sets the reconnection strategy.
    pub fn with_connect_strategy(mut self, strategy: ConnectStrategy) -> Self {
        self.connect_strategy = strategy;
        self
    }

    /// Sets the finalization boundary to resume from.
    pub fn with_root(mut self, checkpoint: Option<Checkpoint<ChainBlockMetadata>>) -> Self {
        self.root = checkpoint;
        self
    }

    /// Sets the last known tip to resume from.
    pub fn with_tip(mut self, checkpoint: Option<Checkpoint<ChainBlockMetadata>>) -> Self {
        self.tip = checkpoint;
        self
    }

    /// Sets the reorg filter half-life. Observed reorg depths accumulate into a threshold that
    /// halves every half-life, filtering smaller reorgs until the threshold decays to zero.
    pub fn with_filter_half_life(mut self, half_life: Duration) -> Self {
        self.filter_half_life = half_life;
        self
    }
}
