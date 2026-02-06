//! Test utilities for the `node` layer.
//!
//! Provides an in-process Kaspa simnet node ([`L1Node`]) and convenience
//! extensions for the L1 bridge ([`L1BridgeExt`]) to simplify integration
//! tests.
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use vprogs_node_l1_bridge::{L1Bridge, L1BridgeConfig, L1Event, NetworkType};
//! use vprogs_node_test_suite::{L1BridgeExt, L1Node};
//!
//! # async fn example() {
//! // Start an isolated simnet node.
//! let node = L1Node::new().await;
//!
//! // Connect a bridge to it.
//! let bridge = L1Bridge::new(
//!     L1BridgeConfig::default()
//!         .with_url(node.wrpc_borsh_url())
//!         .with_network_type(NetworkType::Simnet),
//! );
//!
//! // Wait for the bridge to connect.
//! bridge.wait_for(Duration::from_secs(10), |e| matches!(e, L1Event::Connected)).await;
//!
//! // Mine some blocks and wait for them to arrive.
//! node.mine_blocks(5).await;
//! bridge
//!     .wait_for(Duration::from_secs(10), |e| {
//!         matches!(e, L1Event::ChainBlockAdded { index: 5, .. })
//!     })
//!     .await;
//!
//! bridge.shutdown();
//! node.shutdown().await;
//! # }
//! ```

mod l1_bridge_ext;
mod l1_node;

pub use kaspa_consensus_core::{
    Hash,
    network::{NetworkId, NetworkType},
};
pub use l1_bridge_ext::L1BridgeExt;
pub use l1_node::L1Node;
