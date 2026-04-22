use borsh::{BorshDeserialize, BorshSerialize};

use crate::Hash;

/// Per-block metadata the bridge attaches to each L1 chain block.
///
/// Satisfies the [`BatchMetadata`](vprogs_core_types::BatchMetadata) blanket impl via its derived
/// traits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct ChainBlockMetadata {
    /// L1 block hash.
    pub block_hash: Hash,
    /// DAG blue score at this block's position.
    pub blue_score: u64,
    /// DAA score at this block's position.
    pub daa_score: u64,
    /// Block header timestamp in milliseconds.
    pub timestamp: u64,
    /// Previous block's header timestamp in milliseconds.
    pub prev_timestamp: u64,
    /// Lane key this block's accepted txs are bound to.
    pub lane_key: [u8; 32],
    /// Lane tip entering this block.
    pub prev_lane_tip: [u8; 32],
    /// Lane tip after applying this block's accepted txs.
    pub lane_tip: [u8; 32],
}

impl ChainBlockMetadata {
    /// Positional constructor covering every field.
    pub fn new(
        block_hash: Hash,
        blue_score: u64,
        daa_score: u64,
        timestamp: u64,
        prev_timestamp: u64,
        lane_key: [u8; 32],
        prev_lane_tip: [u8; 32],
        lane_tip: [u8; 32],
    ) -> Self {
        Self {
            block_hash,
            blue_score,
            daa_score,
            timestamp,
            prev_timestamp,
            lane_key,
            prev_lane_tip,
            lane_tip,
        }
    }
}
