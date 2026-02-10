use borsh::{BorshDeserialize, BorshSerialize};

use crate::BlockHash;

/// Persistable metadata extracted from an L1 chain block.
///
/// Contains the fields needed to reconstruct a [`ChainBlock`](crate::ChainBlock) on resume.
/// Implements [`BatchMetadata`](vprogs_core_types::BatchMetadata) automatically via Borsh.
#[derive(Clone, Copy, Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct ChainBlockMetadata {
    hash: BlockHash,
    blue_score: u64,
}

impl ChainBlockMetadata {
    pub fn new(hash: BlockHash, blue_score: u64) -> Self {
        Self { hash, blue_score }
    }

    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    pub fn blue_score(&self) -> u64 {
        self.blue_score
    }
}
