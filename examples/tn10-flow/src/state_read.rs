//! Decoding L2 state into a human number.
//!
//! The transaction-processor guest stores each accessed resource as a little-endian `u32` and
//! increments it by one per transaction (see `zk/backend/risc0/transaction-processor/src/main.rs`).
//! So "the state" of our lane is just one resource's counter, read back after the batch commits.

use vprogs_core_types::ResourceId;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::ReadStore;

/// The single resource every activity transaction on `lane_id` writes to. Derived deterministically
/// so the counter is stable across restarts (layout mirrors `ResourceIdExt::for_test`).
pub fn tracked_resource(lane_id: u32) -> ResourceId {
    let mut bytes = [0u8; 32];
    bytes[24..28].copy_from_slice(&lane_id.to_be_bytes());
    bytes[28..32].copy_from_slice(&1u32.to_be_bytes());
    ResourceId::from(bytes)
}

/// Reads a resource's latest bytes and decodes them as a little-endian `u32` counter. Returns 0
/// when the resource has never been written. Call only after `batch.wait_committed_blocking()`.
pub fn read_resource_u32<S: ReadStore>(store: &S, id: ResourceId) -> u32 {
    let version = StateVersion::from_latest_data(store, id);
    let data = version.data();
    if data.len() >= 4 { u32::from_le_bytes(data[..4].try_into().expect("4 bytes")) } else { 0 }
}
