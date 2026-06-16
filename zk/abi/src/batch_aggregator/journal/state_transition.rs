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
    /// Batch-processor image id this settlement binds to.
    pub batch_image_id: [u8; 32],
    /// `blake2b(perm_redeem_script)` for the bundle's exit output, or `[0u8; 32]` when no
    /// exits were emitted. Non-zero values cause the on-chain settlement to add a second P2SH
    /// output for permission-tree withdrawals.
    pub permission_spk_hash: [u8; 32],
    /// Lane key of the lane this settlement binds to.
    pub lane_key: Hash,
}

impl StateTransition {
    /// Serialized length of the journal in bytes.
    pub const WIRE_SIZE: usize = size_of::<Self>();

    /// Writes the state transition journal to `w` from `args`, in struct field order.
    pub fn encode(w: &mut impl Writer, args: StateTransitionArgs) {
        w.write(args.prev_state);
        w.write(args.prev_lane_tip.as_slice());
        w.write(args.new_state);
        w.write(args.new_lane_tip.as_slice());
        w.write(args.new_seq_commit.as_slice());
        w.write(args.covenant_id);
        w.write(args.tx_image_id);
        w.write(args.batch_image_id);
        w.write(args.permission_spk_hash);
        w.write(args.lane_key.as_slice());
    }
}

/// Borrowed, named input for [`StateTransition::encode`], passed by reference to avoid copies.
pub struct StateTransitionArgs<'a> {
    /// See [`StateTransition::prev_state`].
    pub prev_state: &'a [u8; 32],
    /// See [`StateTransition::prev_lane_tip`].
    pub prev_lane_tip: &'a Hash,
    /// See [`StateTransition::new_state`].
    pub new_state: &'a [u8; 32],
    /// See [`StateTransition::new_lane_tip`].
    pub new_lane_tip: &'a Hash,
    /// See [`StateTransition::new_seq_commit`].
    pub new_seq_commit: &'a Hash,
    /// See [`StateTransition::covenant_id`].
    pub covenant_id: &'a [u8; 32],
    /// See [`StateTransition::tx_image_id`].
    pub tx_image_id: &'a [u8; 32],
    /// See [`StateTransition::batch_image_id`].
    pub batch_image_id: &'a [u8; 32],
    /// See [`StateTransition::permission_spk_hash`].
    pub permission_spk_hash: &'a [u8; 32],
    /// See [`StateTransition::lane_key`].
    pub lane_key: &'a Hash,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_size_matches_encoded_length() {
        let mut buf = Vec::new();
        StateTransition::encode(
            &mut buf,
            StateTransitionArgs {
                prev_state: &[0u8; 32],
                prev_lane_tip: &Hash::default(),
                new_state: &[0u8; 32],
                new_lane_tip: &Hash::default(),
                new_seq_commit: &Hash::default(),
                covenant_id: &[0u8; 32],
                tx_image_id: &[0u8; 32],
                batch_image_id: &[0u8; 32],
                permission_spk_hash: &[0u8; 32],
                lane_key: &Hash::default(),
            },
        );
        assert_eq!(buf.len(), StateTransition::WIRE_SIZE);
        assert_eq!(StateTransition::WIRE_SIZE, 320);
    }
}
