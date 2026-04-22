use borsh::{BorshDeserialize, BorshSerialize};

use crate::Hash;

/// Relevant metadata extracted from an L1 chain block, plus the kip21 seq-commit lane tip
/// computed by the bridge over this block's subnetwork-filtered accepted-tx list.
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
    /// kip21 seq-commit lane tip after this block's accepted-tx list has been applied to the
    /// previous block's lane tip. Computed deterministically by the bridge from public L1 data;
    /// the covenant later uses it to bind a batch proof to the L1-accepted tx set. Zero hash
    /// when the bridge has no subnetwork filter configured (generic-observer mode).
    lane_tip: [u8; 32],
}

impl ChainBlockMetadata {
    /// Creates metadata from the full set of fields extracted from an L1 header plus the
    /// bridge-computed lane tip.
    pub fn new(
        block_hash: Hash,
        blue_score: u64,
        daa_score: u64,
        timestamp: u64,
        selected_parent_timestamp: u64,
        lane_tip: [u8; 32],
    ) -> Self {
        Self { block_hash, blue_score, daa_score, timestamp, selected_parent_timestamp, lane_tip }
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

    /// Returns the post-batch lane tip.
    pub fn lane_tip(&self) -> [u8; 32] {
        self.lane_tip
    }
}
