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
    /// The bundle's deposit address, or `[0u8; 32]` when no tx credited an L1 deposit. Carried
    /// opaquely from the batch journals; the on-chain settlement redeem script decides (gated on
    /// non-zero) what address this covenant accepts. Adjacent to `permission_spk_hash` so the
    /// redeem preimage matches this encode order.
    pub deposit_spk_hash: [u8; 32],
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
        w.write(args.deposit_spk_hash);
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
    /// See [`StateTransition::deposit_spk_hash`].
    pub deposit_spk_hash: &'a [u8; 32],
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
                deposit_spk_hash: &[0u8; 32],
                lane_key: &Hash::default(),
            },
        );
        assert_eq!(buf.len(), StateTransition::WIRE_SIZE);
        assert_eq!(StateTransition::WIRE_SIZE, 352);
    }

    /// The 352-byte preimage byte order the on-chain settlement redeem script assembles
    /// (`build_and_hash_journal`) must equal this `encode` field-for-field; the redeem SHA-256
    /// would otherwise never reproduce the proof's committed journal hash. Pin every 32-byte field
    /// to its absolute offset with distinct sentinels, with explicit focus on the
    /// `deposit_spk_hash` slot landing between `permission_spk_hash` and the trailing `lane_key`
    /// (the field order Unit 2/3 introduced).
    #[test]
    fn encode_field_offsets_match_redeem_preimage_order() {
        let prev_state = [0x01u8; 32];
        let prev_lane_tip = Hash::from_bytes([0x02u8; 32]);
        let new_state = [0x03u8; 32];
        let new_lane_tip = Hash::from_bytes([0x04u8; 32]);
        let new_seq_commit = Hash::from_bytes([0x05u8; 32]);
        let covenant_id = [0x06u8; 32];
        let tx_image_id = [0x07u8; 32];
        let batch_image_id = [0x08u8; 32];
        let permission_spk_hash = [0x09u8; 32];
        let deposit_spk_hash = [0x0au8; 32];
        let lane_key = Hash::from_bytes([0x0bu8; 32]);

        let mut buf = Vec::new();
        StateTransition::encode(
            &mut buf,
            StateTransitionArgs {
                prev_state: &prev_state,
                prev_lane_tip: &prev_lane_tip,
                new_state: &new_state,
                new_lane_tip: &new_lane_tip,
                new_seq_commit: &new_seq_commit,
                covenant_id: &covenant_id,
                tx_image_id: &tx_image_id,
                batch_image_id: &batch_image_id,
                permission_spk_hash: &permission_spk_hash,
                deposit_spk_hash: &deposit_spk_hash,
                lane_key: &lane_key,
            },
        );
        assert_eq!(buf.len(), 352);

        // The exact append order `script.rs::build_and_hash_journal` emits.
        assert_eq!(&buf[0..32], &prev_state);
        assert_eq!(&buf[32..64], prev_lane_tip.as_bytes());
        assert_eq!(&buf[64..96], &new_state);
        assert_eq!(&buf[96..128], new_lane_tip.as_bytes());
        assert_eq!(&buf[128..160], new_seq_commit.as_bytes());
        assert_eq!(&buf[160..192], &covenant_id);
        assert_eq!(&buf[192..224], &tx_image_id);
        assert_eq!(&buf[224..256], &batch_image_id);
        assert_eq!(&buf[256..288], &permission_spk_hash, "permission_spk_hash at 256..288");
        assert_eq!(
            &buf[288..320],
            &deposit_spk_hash,
            "deposit_spk_hash must sit between permission_spk_hash and lane_key (288..320)",
        );
        assert_eq!(&buf[320..352], lane_key.as_bytes(), "lane_key stays last (320..352)");
    }
}
