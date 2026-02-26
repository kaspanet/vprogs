use alloc::vec::Vec;

use rkyv::{Archive, Serialize};

/// Owned witness data for passing to ZK backends.
///
/// Serialized once via rkyv; the guest accesses the archived form zero-copy.
#[derive(Archive, Serialize)]
pub struct Witness {
    pub tx_bytes: Vec<u8>,
    pub tx_index: u32,
    pub batch_metadata: BatchMetadata,
    pub accounts: Vec<Account>,
}

/// Batch-level metadata mirroring [`ChainBlockMetadata`](vprogs_node_l1_bridge::ChainBlockMetadata)
/// in a `no_std`-compatible, rkyv-serializable form.
#[derive(Archive, Serialize)]
pub struct BatchMetadata {
    pub block_hash: [u8; 32],
    pub blue_score: u64,
}

/// A snapshot of a single account's state at execution time.
#[derive(Archive, Serialize)]
pub struct Account {
    pub account_id: Vec<u8>,
    pub data: Vec<u8>,
}
