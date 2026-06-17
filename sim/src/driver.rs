//! The L2 driver: a [`Producer`](crate::l2_miner::Producer) that runs the full L2 stack against
//! the simulated chain.
//!
//! Attached to one miner, it (1) follows that node's selected chain, scheduling each block's
//! lane-activity transactions through the zk `Vm` and handling reorgs via `rollback_to`; (2) issues
//! new seeded lane-activity transactions funded from the miner's coinbase; (3) optionally
//! bootstraps a covenant and periodically settles it (dev settlements, validated by the sim's real
//! script engine); and (4) asserts invariants every block. The strongest is that the decoded L2
//! counter equals the number of lane-activity transactions executed on the current selected chain;
//! a failing invariant panics with the block hash, so a fixed seed pinpoints the bug.

mod config;
mod l2_driver;
mod stats;

pub use config::L2Config;
pub use l2_driver::L2Driver;
pub use stats::DriverStats;
