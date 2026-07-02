use borsh::{BorshDeserialize, BorshSerialize};
use kaspa_rpc_core::{RpcHeader, RpcOptionalHeader};
use vprogs_core_types::BatchMetadata;

use crate::{Hash, SettlementInfo};

/// Per-block metadata the bridge attaches to each L1 chain block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ChainBlockMetadata {
    /// L1 block hash.
    pub hash: Hash,
    /// Canonical id of the parent block (the one this block extends); `0` for the first block.
    pub parent_id: u64,
    /// DAG blue score at this block's position.
    pub blue_score: u64,
    /// DAA score at this block's position.
    pub daa_score: u64,
    /// Block header timestamp in milliseconds.
    pub timestamp: u64,
    /// Previous block's header timestamp in milliseconds.
    pub prev_timestamp: u64,
    /// Lane key this block's accepted txs are bound to. A zero `Hash` means no lane configured.
    pub lane_key: Hash,
    /// Sequencing commitment carried by this block's header.
    pub seq_commit: Hash,
    /// Sequencing commitment carried by the previous block's header.
    pub prev_seq_commit: Hash,
    /// Blue score at which the lane was last active. Zero if never active.
    pub prev_lane_blue_score: u64,
    /// Blue score at which the lane was last active after applying this block's accepted txs.
    pub lane_blue_score: u64,
    /// Lane tip entering this block.
    pub prev_lane_tip: Hash,
    /// Lane tip after applying this block's accepted txs.
    pub lane_tip: Hash,
    /// True when the lane was silent past the finality window at this block.
    pub lane_expired: bool,
    /// Most-recent settlement of the configured covenant, or `None` until one lands.
    pub last_settlement: Option<SettlementInfo>,
}

impl BatchMetadata for ChainBlockMetadata {
    fn block_hash(&self) -> [u8; 32] {
        self.hash.as_bytes()
    }

    fn parent_id(&self) -> u64 {
        self.parent_id
    }
}

impl From<&RpcHeader> for ChainBlockMetadata {
    /// Builds metadata from a regular RPC header; only header fields are set, the rest default.
    fn from(h: &RpcHeader) -> Self {
        Self {
            hash: h.hash,
            blue_score: h.blue_score,
            daa_score: h.daa_score,
            timestamp: h.timestamp,
            seq_commit: h.accepted_id_merkle_root,
            ..Default::default()
        }
    }
}

impl TryFrom<&RpcOptionalHeader> for ChainBlockMetadata {
    /// Name of the first missing required field.
    type Error = &'static str;

    /// Builds metadata from a verbose RPC header; only header fields are set, the rest default.
    ///
    /// Returns the first missing required field's name on a malformed Full-verbosity response.
    fn try_from(h: &RpcOptionalHeader) -> Result<Self, Self::Error> {
        Ok(Self {
            hash: h.hash.ok_or("missing hash")?,
            blue_score: h.blue_score.ok_or("missing blue_score")?,
            daa_score: h.daa_score.ok_or("missing daa_score")?,
            timestamp: h.timestamp.ok_or("missing timestamp")?,
            seq_commit: h.accepted_id_merkle_root.ok_or("missing seq_commit")?,
            ..Default::default()
        })
    }
}
