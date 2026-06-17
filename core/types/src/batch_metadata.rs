use core::fmt::Debug;

use borsh::{BorshDeserialize, BorshSerialize};

/// Opaque metadata attached to each scheduler batch, supporting serialization.
///
/// Implementors derive `BorshSerialize` and `BorshDeserialize` and supply the chain block hash
/// the batch was formed against via [`block_hash`](Self::block_hash), which keys the batch's
/// proof receipts in the proof-receipt store.
pub trait BatchMetadata:
    BorshSerialize + BorshDeserialize + Clone + Debug + Default + Send + Sync + 'static
{
    /// The chain block hash this batch was formed against, as raw bytes.
    ///
    /// The proving workers fold it into each receipt's cache key so the same checkpoint index on
    /// two competing chains yields distinct keys.
    fn block_hash(&self) -> [u8; 32];
}

/// A `u64` batch index doubles as minimal test metadata: its big-endian bytes stand in for a
/// block hash, keeping per-index receipts distinct without a real chain. Gated behind `test-utils`
/// so the node never treats a bare index as batch metadata.
#[cfg(feature = "test-utils")]
impl BatchMetadata for u64 {
    fn block_hash(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&self.to_be_bytes());
        bytes
    }
}
