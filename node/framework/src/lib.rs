//! Generic node framework connecting the L1 bridge to the L2 scheduler.
//!
//! The framework orchestrates the flow of L1 chain blocks through pre-processing (translating
//! blocks into L2 transactions) and scheduling (executing those transactions in parallel). Blocks
//! are pre-processed out of order on execution workers but fed to the scheduler in sequence via
//! [`PreparedBatch`] containers with async latches.
//!
//! # Usage
//!
//! ```ignore
//! use vprogs_node_framework::{Node, NodeConfig};
//!
//! // Configure and start the node.
//! let node = Node::new(
//!     NodeConfig::default()
//!         .with_execution_config(execution_config)
//!         .with_storage_config(storage_config)
//!         .with_l1_bridge_config(l1_bridge_config),
//! );
//!
//! // The node runs autonomously until shut down.
//! node.shutdown();
//! ```

mod config;
mod metadata;
mod node;
mod prepared_batch;
mod vm;
mod worker;

pub use config::NodeConfig;
pub use node::Node;
pub use prepared_batch::PreparedBatch;
pub use vm::NodeVm;
