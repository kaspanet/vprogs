use borsh::{BorshDeserialize, BorshSerialize};

use crate::Hash;

/// Relevant metadata extracted from an L1 chain block.
///
/// Satisfies the [`BatchMetadata`](vprogs_core_types::BatchMetadata) blanket impl via its derived
/// traits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct ChainBlockMetadata {
    /// L1 block hash.
    block_hash: Hash,
    /// DAG blue score at this block's position.
    blue_score: u64,
    /// DAA score at this block's position. Needed by the seq-commit mergeset context hash.
    daa_score: u64,
    /// Block header timestamp (milliseconds). Recorded for completeness / covenant auditability.
    timestamp: u64,
    /// Selected-parent block's header timestamp (milliseconds). This is the value the kip21
    /// sequencing commitment's `mergeset_context_hash` commits to, not the block's own timestamp.
    /// For the genesis / pruning-point-anchor block this equals [`timestamp`](Self::timestamp).
    selected_parent_timestamp: u64,
}

impl ChainBlockMetadata {
    /// Creates metadata from the full set of fields extracted from an L1 header.
    pub fn new(
        block_hash: Hash,
        blue_score: u64,
        daa_score: u64,
        timestamp: u64,
        selected_parent_timestamp: u64,
    ) -> Self {
        Self { block_hash, blue_score, daa_score, timestamp, selected_parent_timestamp }
    }

    /// Returns the L1 block hash.
    pub fn block_hash(&self) -> Hash {
        self.block_hash
    }

    /// Returns the DAG blue score.
    pub fn blue_score(&self) -> u64 {
        self.blue_score
    }

    /// Returns the DAA score.
    pub fn daa_score(&self) -> u64 {
        self.daa_score
    }

    /// Returns the block header's own timestamp.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Returns the selected parent's timestamp (the value used for the seq-commit context hash).
    pub fn selected_parent_timestamp(&self) -> u64 {
        self.selected_parent_timestamp
    }
}
