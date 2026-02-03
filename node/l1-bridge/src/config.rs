use kaspa_consensus_core::network::{NetworkId, NetworkType};
use kaspa_wrpc_client::prelude::ConnectStrategy;

use crate::ChainCoordinate;

/// Configuration for the L1 bridge.
#[derive(Clone, Debug)]
pub struct L1BridgeConfig {
    /// WebSocket URL to connect to (e.g., "ws://localhost:17110").
    /// If None, uses the Resolver to find a public node.
    pub url: Option<String>,
    /// Network identifier (mainnet, testnet, devnet, simnet).
    pub network_id: NetworkId,
    /// Connection timeout in milliseconds.
    pub connect_timeout_ms: u64,
    /// Reconnection strategy.
    pub connect_strategy: ConnectStrategy,
    /// Last pruned coordinate (finalization threshold), or None to start from the L1 pruning
    /// point. When set together with `last_processed`, the bridge will recover the gap between
    /// the two on startup using a lightweight (non-verbose) sync.
    pub last_pruned: Option<ChainCoordinate>,
    /// Last processed coordinate, or None to start from the pruning point.
    pub last_processed: Option<ChainCoordinate>,
}

impl Default for L1BridgeConfig {
    fn default() -> Self {
        Self {
            url: None,
            network_id: NetworkId::new(NetworkType::Mainnet),
            connect_timeout_ms: 10_000,
            connect_strategy: ConnectStrategy::Retry,
            last_pruned: None,
            last_processed: None,
        }
    }
}

impl L1BridgeConfig {
    /// Creates a new configuration with a specific URL.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Sets the network ID.
    pub fn with_network_id(mut self, network_id: NetworkId) -> Self {
        self.network_id = network_id;
        self
    }

    /// Sets the network type (creates a NetworkId from it).
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

    /// Sets the last pruned coordinate (finalization threshold).
    pub fn with_last_pruned(mut self, coord: Option<ChainCoordinate>) -> Self {
        self.last_pruned = coord;
        self
    }

    /// Sets the last processed coordinate.
    pub fn with_last_processed(mut self, coord: Option<ChainCoordinate>) -> Self {
        self.last_processed = coord;
        self
    }
}
