use borsh::{BorshDeserialize, BorshSerialize};
use kaspa_rpc_core::RpcOptionalHeader;

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
    pub lane_key: Hash,
    /// Sequencing commitment carried by this block's header.
    pub seq_commit: Hash,
    /// Sequencing commitment carried by the previous block's header.
    pub prev_seq_commit: Hash,
    /// Blue score at which the lane was last active. Zero if never active.
    pub lane_blue_score: u64,
    /// Lane tip entering this block.
    pub prev_lane_tip: Hash,
    /// Lane tip after applying this block's accepted txs.
    pub lane_tip: Hash,
    /// True when the lane was silent past the finality window at this block.
    pub lane_expired: bool,
}

impl From<&RpcOptionalHeader> for ChainBlockMetadata {
    /// Builds metadata from a verbose RPC header, populating only the header-derived fields and
    /// leaving lane / parent-derived state at its default. Panics if a required field is absent,
    /// which only happens when the kaspa node returns a malformed Full-verbosity response.
    fn from(h: &RpcOptionalHeader) -> Self {
        Self {
            hash: h.hash.expect("missing hash"),
            blue_score: h.blue_score.expect("missing blue_score"),
            daa_score: h.daa_score.expect("missing daa_score"),
            timestamp: h.timestamp.expect("missing timestamp"),
            seq_commit: h.accepted_id_merkle_root.expect("missing seq_commit"),
            ..Default::default()
        }
    }
}
