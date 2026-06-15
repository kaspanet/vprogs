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
    /// Lane tip entering this batch's block.
    pub prev_lane_tip: Hash,
    /// Blue score at which the lane was last active before this batch.
    pub prev_lane_blue_score: zerocopy::little_endian::U64,
    /// L2 SMT state root after this batch.
    pub new_state: [u8; 32],
    /// Lane tip after this batch.
    pub new_lane_tip: Hash,
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
    /// Writes the fixed header followed by the raw exits blob, in struct field order.
    pub fn encode(w: &mut impl Writer, args: BatchTransitionArgs) {
        w.write(args.prev_state);
        w.write(args.prev_lane_tip.as_slice());
        w.write(&args.prev_lane_blue_score.to_le_bytes());
        w.write(args.new_state);
        w.write(args.new_lane_tip.as_slice());
        w.write(&args.new_lane_blue_score.to_le_bytes());
        w.write(args.lane_key.as_slice());
        w.write(args.covenant_id);
        w.write(args.tx_image_id);
        w.write(&[args.lane_expired as u8]);
        w.write(args.exits);
    }
}

/// Borrowed, named input for [`BatchTransition::encode`].
pub struct BatchTransitionArgs<'a> {
    /// See [`BatchTransition::prev_state`].
    pub prev_state: &'a [u8; 32],
    /// See [`BatchTransition::prev_lane_tip`].
    pub prev_lane_tip: &'a Hash,
    /// See [`BatchTransition::prev_lane_blue_score`].
    pub prev_lane_blue_score: u64,
    /// See [`BatchTransition::new_state`].
    pub new_state: &'a [u8; 32],
    /// See [`BatchTransition::new_lane_tip`].
    pub new_lane_tip: &'a Hash,
    /// See [`BatchTransition::new_lane_blue_score`].
    pub new_lane_blue_score: u64,
    /// See [`BatchTransition::lane_key`].
    pub lane_key: &'a Hash,
    /// See [`BatchTransition::covenant_id`].
    pub covenant_id: &'a [u8; 32],
    /// See [`BatchTransition::tx_image_id`].
    pub tx_image_id: &'a [u8; 32],
    /// See [`BatchTransition::lane_expired`].
    pub lane_expired: bool,
    /// Encoded [`BatchTransition::exits`] blob.
    pub exits: &'a [u8],
}
