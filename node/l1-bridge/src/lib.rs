mod bridge;
mod config;
mod error;
pub mod event;
mod event_queue;
mod state;

pub use bridge::L1Bridge;
pub use config::BridgeConfig;
pub use error::{L1BridgeError, Result};
pub use event::{
    BlockAdded, BlockHash, DaaScoreChanged, FinalityConflict, FinalityResolved, L1Event,
    VirtualChainChanged,
};
pub use event_queue::EventQueue;
pub use kaspa_consensus_core::network::{NetworkId, NetworkType};
pub use kaspa_wrpc_client::prelude::ConnectStrategy;
pub use state::ConnectionState;
