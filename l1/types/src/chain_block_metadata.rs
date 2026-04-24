use borsh::{BorshDeserialize, BorshSerialize};

use crate::Hash;

/// Per-block metadata the bridge attaches to each L1 chain block.
///
/// Satisfies the [`BatchMetadata`](vprogs_core_types::BatchMetadata) blanket impl via its derived
/// traits.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
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
    /// Parent block's sequencing commitment (enables settlement chains to bind prev→new
    /// across successive batches).
    pub prev_seq_commit: Hash,
    /// Blue score at which the lane was last active. Zero if never active.
    pub lane_blue_score: u64,
    /// Lane tip entering this block.
    pub prev_lane_tip: [u8; 32],
    /// Lane tip after applying this block's accepted txs.
    pub lane_tip: [u8; 32],
    /// True when the lane was silent past the finality window at this block and the next
    /// activity re-anchors on `prev_seq_commit` instead of `prev_lane_tip` (kip21 §5.1).
    pub lane_expired: bool,
    /// `miner_payload_leaf(merged_block_hash, blue_work, coinbase_payload)` for every block
    /// in this chain block's mergeset. Populated from the kaspa v2 RPC `lane_data` bundle;
    /// empty when no lane was queried.
    pub miner_payload_leaves: Vec<Hash>,
    /// Serialized `kaspa_smt::proof::OwnedSmtProof` for the bridge's `lane_key` position in
    /// this block's `lanes_root`. Empty when no lane was queried.
    pub lane_smt_proof: Vec<u8>,
}
