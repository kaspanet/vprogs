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
    /// `blake2b(perm_redeem_script)` for the bundle's exit output, or `[0u8; 32]` when no
    /// exits were emitted. Non-zero values cause the on-chain settlement to add a second P2SH
    /// output for permission-tree withdrawals.
    pub permission_spk_hash: [u8; 32],
    /// Kaspa SubnetworkId this settlement binds to (the lane the batch processor was built for).
    /// The covenant SPK pins the same 20 bytes in its redeem-script prefix, so any settlement
    /// proof that names a different lane fails the SHA-256 journal-preimage check.
    pub subnetwork_id: [u8; 20],
}

/// Serialized length of [`StateTransition`] in bytes.
pub const JOURNAL_SIZE: usize = 32 * 8 + 20;

impl StateTransition {
    /// Writes the state transition journal to `w`.
    pub fn encode(
        w: &mut impl Writer,
        (prev_state, prev_lane_tip): (&[u8; 32], &Hash),
        (new_state, new_lane_tip, new_seq_commit): (&[u8; 32], &Hash, &Hash),
        covenant_id: &[u8; 32],
        tx_image_id: &[u8; 32],
        permission_spk_hash: &[u8; 32],
        subnetwork_id: &[u8; 20],
    ) {
        w.write(prev_state);
        w.write(prev_lane_tip.as_slice());
        w.write(new_state);
        w.write(new_lane_tip.as_slice());
        w.write(new_seq_commit.as_slice());
        w.write(covenant_id);
        w.write(tx_image_id);
        w.write(permission_spk_hash);
        w.write(subnetwork_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_size_matches_encoded_length() {
        let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        StateTransition::encode(
            &mut buf,
            (&[0u8; 32], &Hash::default()),
            (&[0u8; 32], &Hash::default(), &Hash::default()),
            &[0u8; 32],
            &[0u8; 32],
            &[0u8; 32],
            &[0u8; 20],
        );
        assert_eq!(buf.len(), JOURNAL_SIZE);
        assert_eq!(JOURNAL_SIZE, 276);
    }
}
