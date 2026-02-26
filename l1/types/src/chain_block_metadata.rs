use borsh::{BorshDeserialize, BorshSerialize};

use crate::BlockHash;

/// Relevant metadata extracted from an L1 chain block.
///
/// Satisfies the [`BatchMetadata`](vprogs_core_types::BatchMetadata) blanket impl via its derived
/// traits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ChainBlockMetadata {
    /// L1 block hash.
    hash: BlockHash,
    /// DAG blue score at this block's position.
    blue_score: u64,
}

impl ChainBlockMetadata {
    /// Creates metadata from a block hash and blue score.
    pub fn new(hash: BlockHash, blue_score: u64) -> Self {
        Self { hash, blue_score }
    }

    /// Returns the L1 block hash.
    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    /// Returns the DAG blue score.
    pub fn blue_score(&self) -> u64 {
        self.blue_score
    }
}
