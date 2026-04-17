use borsh::{BorshDeserialize, BorshSerialize};

use crate::Hash;

/// Relevant metadata extracted from an L1 chain block.
///
/// Satisfies the `vprogs_core_types::BatchMetadata` blanket impl via its derived traits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct ChainBlockMetadata {
    /// L1 block hash.
    block_hash: Hash,
    /// DAG blue score at this block's position.
    blue_score: u64,
}

impl ChainBlockMetadata {
    /// Creates metadata from a block hash and blue score.
    pub fn new(block_hash: Hash, blue_score: u64) -> Self {
        Self { block_hash, blue_score }
    }

    /// Returns the L1 block hash.
    pub fn block_hash(&self) -> Hash {
        self.block_hash
    }

    /// Returns the DAG blue score.
    pub fn blue_score(&self) -> u64 {
        self.blue_score
    }
}
