use kaspa_hashes::Hash;
use vprogs_core_codec::Writer;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

/// Bundle state transition journal.
#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct StateTransition {
    /// L2 SMT state root before the bundle.
    pub prev_state: [u8; 32],
    /// Lane tip entering the bundle.
    pub prev_lane_tip: Hash,
    /// L2 SMT state root after the bundle.
    pub new_state: [u8; 32],
    /// Lane tip after the bundle.
    pub new_lane_tip: Hash,
    /// Block-header `seq_commit` derived from `new_lane_tip` and the lane proof.
    pub new_seq_commit: Hash,
    /// Covenant id this settlement binds to.
    pub covenant_id: [u8; 32],
    /// Transaction-processor image id this settlement binds to.
    pub tx_image_id: [u8; 32],
}

impl StateTransition {
    /// Writes the state transition journal to `w`.
    pub fn encode(
        w: &mut impl Writer,
        (prev_state, prev_lane_tip): (&[u8; 32], &Hash),
        (new_state, new_lane_tip, new_seq_commit): (&[u8; 32], &Hash, &Hash),
        covenant_id: &[u8; 32],
        tx_image_id: &[u8; 32],
    ) {
        w.write(prev_state);
        w.write(prev_lane_tip.as_slice());
        w.write(new_state);
        w.write(new_lane_tip.as_slice());
        w.write(new_seq_commit.as_slice());
        w.write(covenant_id);
        w.write(tx_image_id);
    }
}
