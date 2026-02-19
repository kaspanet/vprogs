//! Generic node framework connecting the L1 bridge to the L2 scheduler.
//!
//! The framework orchestrates the flow of L1 chain blocks through pre-processing (translating
//! blocks into L2 transactions) and scheduling (executing those transactions in parallel). Blocks
//! are pre-processed out of order on execution workers but fed to the scheduler in sequence using
//! async latches to preserve block ordering.
//!
//! # Usage
//!
//! ```ignore
//! use vprogs_node_framework::{Node, NodeConfig};
//!
//! // Configure and start the node.
//! let config = NodeConfig::default()
//!     .with_execution_config(execution_config)
//!     .with_storage_config(storage_config)
//!     .with_l1_bridge_config(l1_bridge_config);
//! let node = Node::new(config);
//!
//! // Query state directly (lock-free, synchronous via SchedulerState deref).
//! let checkpoint = node.api().last_committed();
//!
//! // Operations needing &mut Scheduler go through the worker thread.
//! let threshold = node.api().with_scheduler(|s| s.pruning().threshold()).await?;
//!
//! // The node runs autonomously until shut down.
//! node.shutdown();
//! ```

mod api;
mod batch;
mod config;
mod error;
mod node;
mod vm;
mod worker;

pub use api::NodeApi;
pub use config::NodeConfig;
pub use error::{NodeError, NodeResult};
pub use node::Node;
pub use vm::NodeVm;
