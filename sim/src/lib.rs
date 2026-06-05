//! Seeded, long-running L2 simulation harness.
//!
//! Drives a multi-node simulated kaspa network (reusing the `kaspa_utils::sim` discrete-event
//! engine, real [`Consensus`](kaspa_consensus::consensus::Consensus) nodes, and simpa's filler
//! miners) and runs the full L2 stack — covenant bootstrap, seeded lane activity, scheduler/Vm
//! execution, and settlement — against the produced chain, asserting invariants as it goes. A fixed
//! seed makes any failure reproducible.

pub mod config;
pub mod driver;
pub mod l2_miner;
pub mod lane_source;
pub mod network;
