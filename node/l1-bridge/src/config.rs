use kaspa_consensus_core::network::{NetworkId, NetworkType};
use kaspa_wrpc_client::prelude::ConnectStrategy;

/// Configuration for the L1 bridge connection.
#[derive(Clone, Debug)]
pub struct L1BridgeConfig {
    /// WebSocket URL to connect to (e.g., "ws://localhost:17110").
    /// If None, uses the Resolver to find a public node.
    pub url: Option<String>,
    /// Network identifier (mainnet, testnet, devnet, simnet).
    pub network_id: NetworkId,
    /// Connection timeout in milliseconds.
    pub connect_timeout_ms: u64,
    /// Whether to block on initial connection.
    pub block_async_connect: bool,
    /// Reconnection strategy.
    pub connect_strategy: ConnectStrategy,
}

impl Default for L1BridgeConfig {
    fn default() -> Self {
        Self {
            url: None,
            network_id: NetworkId::new(NetworkType::Mainnet),
            connect_timeout_ms: 10_000,
            block_async_connect: false,
            connect_strategy: ConnectStrategy::Retry,
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

    /// Sets whether to block on initial connection.
    pub fn with_blocking_connect(mut self, block: bool) -> Self {
        self.block_async_connect = block;
        self
    }

    /// Sets the reconnection strategy.
    pub fn with_connect_strategy(mut self, strategy: ConnectStrategy) -> Self {
        self.connect_strategy = strategy;
        self
    }
}
