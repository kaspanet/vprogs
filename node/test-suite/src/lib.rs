//! Test utilities for the node domain.
//!
//! Provides infrastructure for spawning local Kaspa simnet nodes and networks
//! for integration testing. Supports simulating forks and reorgs.

mod simnet_network;
mod simnet_node;

pub use kaspa_consensus_core::Hash;
// Re-export commonly used types for convenience
pub use kaspa_consensus_core::network::{NetworkId, NetworkType};
pub use simnet_network::SimnetNetwork;
pub use simnet_node::{SimnetNode, default_args};
