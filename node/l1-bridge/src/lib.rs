mod bridge;
mod config;
mod error;
mod event;
mod state;

pub use bridge::{EventQueue, L1Bridge};
pub use config::BridgeConfig;
pub use error::{L1BridgeError, Result};
pub use event::{
    BlockAdded, BlockHash, DaaScoreChanged, FinalityConflict, FinalityResolved, L1Event,
    VirtualChainChanged,
};
// Re-export commonly used types from kaspa crates for convenience.
pub use kaspa_consensus_core::network::{NetworkId, NetworkType};
pub use kaspa_wrpc_client::prelude::ConnectStrategy;
pub use state::ConnectionState;
