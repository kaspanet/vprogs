//! Event-driven bridge to the Kaspa L1 network.
//!
//! Spawns a background worker thread that connects to an L1 node over wRPC,
//! tracks the selected parent chain, and emits [`L1Event`]s through a lock-free
//! queue. The bridge handles reconnection, reorgs, and finalization
//! automatically.
//!
//! # Usage
//!
//! ```no_run
//! use vprogs_node_l1_bridge::{L1Bridge, L1BridgeConfig, L1Event, NetworkType};
//!
//! let bridge = L1Bridge::new(
//!     L1BridgeConfig::default()
//!         .with_url("ws://localhost:17110")
//!         .with_network_type(NetworkType::Mainnet),
//! );
//!
//! // Consume events in a loop.
//! loop {
//!     match bridge.pop() {
//!         Some(L1Event::Connected) => println!("connected"),
//!         Some(L1Event::ChainBlockAdded { index, .. }) => {
//!             println!("block {index}");
//!         }
//!         Some(L1Event::Rollback(index)) => println!("rollback to {index}"),
//!         Some(L1Event::Finalized(block)) => {
//!             println!("finalized up to index {}", block.index());
//!         }
//!         Some(L1Event::Disconnected) => println!("disconnected"),
//!         Some(L1Event::Fatal { reason }) => {
//!             eprintln!("fatal: {reason}");
//!             break;
//!         }
//!         None => {}
//!     }
//! }
//!
//! bridge.shutdown();
//! ```
//!
//! For async consumers, [`L1Bridge::wait_and_pop`] blocks until an event is
//! available. [`L1Bridge::drain`] returns all currently queued events at once.
//!
//! # Resuming
//!
//! To resume from a previously known chain position, pass both `root` and `tip`
//! in the config. The bridge will backfill the chain between them on first
//! connect before emitting new events.

mod bridge;
mod chain_block;
mod config;
mod error;
mod event;
mod virtual_chain;
mod worker;

pub use bridge::L1Bridge;
pub use chain_block::ChainBlock;
pub use config::L1BridgeConfig;
pub use event::{Hash as BlockHash, L1Event, RpcOptionalHeader, RpcOptionalTransaction};
pub use kaspa_consensus_core::network::{NetworkId, NetworkType};
pub use kaspa_wrpc_client::prelude::ConnectStrategy;
