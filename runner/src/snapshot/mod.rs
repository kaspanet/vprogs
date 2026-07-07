//! Snapshot save (and, in a later branch, restore) for bringing up a fresh node past pruning.
pub mod header;
pub mod save;

pub use header::SnapshotHeader;
