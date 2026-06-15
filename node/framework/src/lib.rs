//! Generic node framework connecting the L1 bridge to the scheduler.
//!
//! The framework orchestrates the flow of L1 chain blocks through the scheduler, extracting
//! access metadata from L1 transaction payloads and scheduling them for parallel execution.
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
mod config;
mod error;
mod node;
mod processor;
mod worker;

pub use api::NodeApi;
pub use config::NodeConfig;
pub use error::{NodeError, NodeResult};
pub use node::Node;
pub use processor::Processor;
