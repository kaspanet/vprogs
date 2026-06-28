use core::fmt::Debug;

use borsh::{BorshDeserialize, BorshSerialize};

/// Metadata attached to each scheduler batch.
pub trait BatchMetadata:
    BorshSerialize + BorshDeserialize + Clone + Debug + Default + Send + Sync + 'static
{
    /// The block hash this batch corresponds to.
    fn block_hash(&self) -> [u8; 32];

    /// Canonical id of the parent batch (the one this batch extends), or `0` for the first batch.
    fn parent_id(&self) -> u64;
}

/// Lightweight metadata for tests and simple deployments: the value zero-padded into a hash.
impl BatchMetadata for u64 {
    fn block_hash(&self) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[24..].copy_from_slice(&self.to_be_bytes());
        hash
    }

    fn parent_id(&self) -> u64 {
        0
    }
}
