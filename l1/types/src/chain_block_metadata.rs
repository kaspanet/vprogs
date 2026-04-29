use borsh::{BorshDeserialize, BorshSerialize};

use crate::Hash;

/// Per-block metadata the bridge attaches to each L1 chain block.
///
/// Satisfies the [`BatchMetadata`](vprogs_core_types::BatchMetadata) blanket impl via its derived
/// traits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ChainBlockMetadata {
    /// L1 block hash.
    pub hash: Hash,
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
    /// Sequencing commitment carried by this block's header.
    pub seq_commit: Hash,
    /// Chain parent's `seq_commit` — feeds the guest's lane_expired re-anchor when this
    /// block's section has `lane_expired = true`.
    pub prev_seq_commit: Hash,
    /// Blue score at which the lane was last active. Zero if never active.
    pub lane_blue_score: u64,
    /// Lane tip entering this block.
    pub prev_lane_tip: [u8; 32],
    /// Lane tip after applying this block's accepted txs.
    pub lane_tip: [u8; 32],
    /// True when the lane was silent past the finality window at this block.
    pub lane_expired: bool,
}
