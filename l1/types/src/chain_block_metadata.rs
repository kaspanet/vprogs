use borsh::{BorshDeserialize, BorshSerialize};

use crate::Hash;

/// Per-block metadata the bridge attaches to each L1 chain block.
///
/// Carries only per-block context — fields that the batch prover needs once per chain block
/// (for `lane_tip_next`, `mergeset_context_hash`, the lane_expired re-anchor path, and
/// tx-receipt cross-checks). Settlement-only inputs (`payload_and_ctx_digest`,
/// `lane_smt_proof`, and the *final-block* `parent_seq_commit` used in the kip21 seq_commit
/// derivation) are fetched on demand at bundle boundary via `get_seq_commit_lane_proof`.
///
/// Note that `prev_seq_commit` here is the chain *parent's* `seq_commit` for *this* block —
/// per-block info used by the guest's lane_expired re-anchor (`parent_ref =
/// parent_seq_commit`). It is unrelated to the settlement-context `parent_seq_commit` (which
/// is the bundle's final block's chain parent seq_commit, used once for kip21).
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
    /// Chain parent's `seq_commit` — feeds the guest's lane_expired re-anchor when this
    /// block's section has `lane_expired = true`.
    pub prev_seq_commit: Hash,
    /// Blue score at which the lane was last active. Zero if never active.
    pub lane_blue_score: u64,
    /// Lane tip entering this block.
    pub prev_lane_tip: [u8; 32],
    /// Lane tip after applying this block's accepted txs.
    pub lane_tip: [u8; 32],
    /// True when the lane was silent past the finality window at this block and the next
    /// activity re-anchors on the parent block's `seq_commit` instead of `prev_lane_tip`
    /// (kip21 §5.1).
    pub lane_expired: bool,
}
