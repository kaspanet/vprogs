use alloc::vec::Vec;

use rkyv::{Archive, Serialize};

use crate::{Account, BatchMetadata};

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
