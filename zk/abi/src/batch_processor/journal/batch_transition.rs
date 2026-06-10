use kaspa_hashes::Hash;
use vprogs_core_codec::Writer;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

use crate::withdrawal::Exits;

/// Per-batch state-transition journal.
#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct BatchTransition {
    /// L2 SMT state root before this batch.
    pub prev_state: [u8; 32],
    /// L2 SMT state root after this batch.
    pub new_state: [u8; 32],
    /// Lane tip entering this batch's block.
    pub prev_lane_tip: Hash,
    /// Lane tip after this batch.
    pub new_lane_tip: Hash,
    /// Blue score at which the lane was last active before this batch.
    pub prev_lane_blue_score: zerocopy::little_endian::U64,
    /// Blue score at which the lane was last active after this batch.
    pub new_lane_blue_score: zerocopy::little_endian::U64,
    /// Lane subnetwork-id hash; constant across the bundle.
    pub lane_key: Hash,
    /// Covenant id this batch settles into; constant across the bundle.
    pub covenant_id: [u8; 32],
    /// Transaction-processor image id this batch was verified against; constant across the bundle.
    pub tx_image_id: [u8; 32],
    /// `1` if the lane re-anchored on `prev_seq_commit` instead of `prev_lane_tip`.
    pub lane_expired: u8,
    /// Per-tx exit emissions, concatenated in journal order.
    pub exits: Exits,
}

impl BatchTransition {
    /// Writes the fixed header followed by the raw exits blob.
    pub fn encode(
        w: &mut impl Writer,
        (prev_state, prev_lane_tip, prev_lane_blue_score): (&[u8; 32], &Hash, u64),
        (new_state, new_lane_tip, new_lane_blue_score): (&[u8; 32], &Hash, u64),
        (lane_key, covenant_id, tx_image_id): (&Hash, &[u8; 32], &[u8; 32]),
        lane_expired: bool,
        exits: &[u8],
    ) {
        w.write(prev_state);
        w.write(new_state);
        w.write(prev_lane_tip.as_slice());
        w.write(new_lane_tip.as_slice());
        w.write(&prev_lane_blue_score.to_le_bytes());
        w.write(&new_lane_blue_score.to_le_bytes());
        w.write(lane_key.as_slice());
        w.write(covenant_id);
        w.write(tx_image_id);
        w.write(&[lane_expired as u8]);
        w.write(exits);
    }
}
