use core::fmt::Debug;

use borsh::{BorshDeserialize, BorshSerialize};

/// Metadata attached to each scheduler batch.
pub trait BatchMetadata:
    BorshSerialize + BorshDeserialize + Clone + Debug + Default + Send + Sync + 'static
{
    /// The block hash this batch corresponds to.
    fn block_hash(&self) -> [u8; 32];
}

/// Lightweight metadata for tests and simple deployments: the value zero-padded into a hash.
impl BatchMetadata for u64 {
    fn block_hash(&self) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[24..].copy_from_slice(&self.to_be_bytes());
        hash
    }
}
