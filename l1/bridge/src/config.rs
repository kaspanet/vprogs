use std::{
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use kaspa_consensus_core::{config::params::Params, subnets::SubnetworkId};
use tokio::sync::watch;
use vprogs_l1_types::{ConnectStrategy, Hash, NetworkId, NetworkType, SettlementInfo};

/// Configuration for the L1 bridge.
#[derive(Clone, Debug)]
pub struct L1BridgeConfig {
    /// WebSocket URL (e.g. `ws://localhost:17110`), or `None` to use the public resolver.
    pub url: Option<String>,
    /// Target network (mainnet, testnet, devnet, simnet).
    pub network_id: NetworkId,
    /// Connection timeout in milliseconds.
    pub connect_timeout_ms: u64,
    /// First-time connect behavior: `Retry` blocks until connected, `Fallback` fails fast.
    pub connect_strategy: ConnectStrategy,
    /// Reorg filter half-life. Observed reorg depths accumulate into a threshold that halves
    /// every half-life until it decays to zero. Set to `Duration::ZERO` to disable.
    pub filter_half_life: Duration,
    /// If set, the bridge only emits transactions whose `subnetwork_id` matches. `None` emits
    /// every accepted transaction unfiltered (generic-observer mode).
    pub subnetwork_id: Option<SubnetworkId>,
    /// Blue-score window within which a lane stays active without new transactions.
    pub finality_depth: u64,
    /// Covenant id tracked by [`ChainBlockMetadata::last_settlement`], or `None` to disable.
    pub covenant_id: Option<Hash>,
    /// On a fresh chain, seed the root this many chain-blocks below the sink, so the bridge starts
    /// near the tip. `None` seeds from the pruning point.
    pub seed_depth: Option<u64>,
    /// On a fresh chain (no `root`/`tip`), the explicit block to seed the root at, instead of the
    /// sink or the pruning point: a known historical block (a covenant's deploy block) a catch-up
    /// node rebuilds state forward from. Fresh-chain precedence: `start_from` > `seed_depth` >
    /// pruning point. `None` defers to the lower-precedence options.
    pub start_from: Option<Hash>,
    /// Optional observer the bridge publishes its latest chain-block DAA score into, for an
    /// external progress reporter; `None` disables publishing.
    pub tip_daa: Option<Arc<AtomicU64>>,
    /// Optional `watch` sender the bridge publishes the tip's last covenant settlement into. The
    /// bridge is the single writer; each settler holds a [`watch::Receiver`] it borrows (and, once
    /// confirmation is notification-based, awaits) to reconcile against the canonical settlement
    /// without a confirm RTT. `None` disables publishing.
    pub settlement_observer: Option<watch::Sender<Option<SettlementInfo>>>,
}

impl Default for L1BridgeConfig {
    /// Defaults to mainnet with a 10-second timeout and blocking initial connect.
    fn default() -> Self {
        Self {
            url: None, // Use the public resolver.
            network_id: NetworkId::new(NetworkType::Mainnet),
            connect_timeout_ms: 10_000,
            connect_strategy: ConnectStrategy::Retry, // Block until the first connect succeeds.
            filter_half_life: Duration::from_secs(3600), // 1 hour.
            subnetwork_id: None,
            finality_depth: Params::from(NetworkId::new(NetworkType::Mainnet)).finality_depth(),
            covenant_id: None,
            seed_depth: None, // Replay from the pruning point by default.
            start_from: None, // No explicit seed block; defer to seed_depth/pruning point.
            tip_daa: None,
            settlement_observer: None,
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

    /// Sets the first-time connect strategy (block-until-connected vs. fail-fast).
    pub fn with_connect_strategy(mut self, strategy: ConnectStrategy) -> Self {
        self.connect_strategy = strategy;
        self
    }

    /// Sets the reorg filter half-life. Observed reorg depths accumulate into a threshold that
    /// halves every half-life, filtering smaller reorgs until the threshold decays to zero.
    pub fn with_filter_half_life(mut self, half_life: Duration) -> Self {
        self.filter_half_life = half_life;
        self
    }

    /// Restricts emitted transactions to a specific subnetwork. `None` means "no filter, emit
    /// every accepted transaction" (the generic-observer default).
    pub fn with_subnetwork_id(mut self, subnetwork_id: Option<SubnetworkId>) -> Self {
        self.subnetwork_id = subnetwork_id;
        self
    }

    /// Sets the lane-expiration blue-score window.
    pub fn with_finality_depth(mut self, finality_depth: u64) -> Self {
        self.finality_depth = finality_depth;
        self
    }

    /// Sets the covenant id to track settlements for. `None` disables covenant tracking.
    pub fn with_covenant_id(mut self, covenant_id: Option<Hash>) -> Self {
        self.covenant_id = covenant_id;
        self
    }

    /// On a fresh chain, seed the root `seed_depth` chain-blocks below the sink instead of from the
    /// pruning point. `None` seeds from the pruning point.
    pub fn with_seed_depth(mut self, seed_depth: Option<u64>) -> Self {
        self.seed_depth = seed_depth;
        self
    }

    /// On a fresh chain, seed the root at this explicit block instead of from the sink or the
    /// pruning point. `None` defers to `seed_depth`/pruning point. Precedence on a fresh chain:
    /// `start_from` > `seed_depth` > pruning point.
    pub fn with_start_from(mut self, start_from: Option<Hash>) -> Self {
        self.start_from = start_from;
        self
    }

    /// Sets the observer the bridge publishes its latest chain-block DAA score into. `None`
    /// disables publishing.
    pub fn with_tip_daa_observer(mut self, tip_daa: Option<Arc<AtomicU64>>) -> Self {
        self.tip_daa = tip_daa;
        self
    }

    /// Sets the `watch` sender the bridge publishes the tip's last covenant settlement into. `None`
    /// disables publishing.
    pub fn with_settlement_observer(
        mut self,
        settlement_observer: Option<watch::Sender<Option<SettlementInfo>>>,
    ) -> Self {
        self.settlement_observer = settlement_observer;
        self
    }
}
