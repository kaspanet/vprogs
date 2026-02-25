use alloc::vec::Vec;

use rkyv::{Archive, Serialize};

/// Owned witness data for passing to ZK backends.
///
/// Serialized once via rkyv; the guest accesses the archived form zero-copy.
#[derive(Archive, Serialize)]
pub struct Witness {
    pub tx_bytes: Vec<u8>,
    pub tx_index: u32,
    pub batch_metadata: Vec<u8>,
    pub accounts: Vec<Account>,
}

/// A snapshot of a single account's state at execution time.
#[derive(Archive, Serialize)]
pub struct Account {
    pub account_id: Vec<u8>,
    pub data: Vec<u8>,
    pub version: u64,
}
