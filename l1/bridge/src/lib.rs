//! Bridge to the Kaspa L1 network.
//!
//! Spawns a background worker thread that connects to an L1 node over wRPC, tracks the selected
//! parent chain, and drives a [`ChainSink`](vprogs_core_types::ChainSink) (the scheduler) directly:
//! it schedules each new chain block, maps reorgs onto the scheduler's never-reused id space, and
//! advances finalization. It also applies API commands against the scheduler, interleaved with L1
//! processing, and handles reconnection automatically. Events (`Connected` / `Disconnected` /
//! `Fatal`) are delivered through a lock-free queue for observers via [`L1Bridge::pop`] /
//! [`L1Bridge::wait_and_pop`] / [`L1Bridge::drain`].
//!
//! On resume the scheduler restores its canonical chain from storage, so the bridge simply adopts
//! its tip and fetches forward from there -- there is no separate backfill.

mod bridge;
mod command;
mod config;
mod error;
mod event;
mod reorg_filter;
mod worker;

pub use bridge::L1Bridge;
pub use command::Command;
pub use config::L1BridgeConfig;
pub use event::L1Event;
