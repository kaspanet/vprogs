use std::io::{Read, Write};

use borsh::{BorshDeserialize, BorshSerialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned, little_endian::U64};

use crate::{Hash, TransactionId};

/// Decoded view of a settlement transaction observed on L1.
///
/// Bundles the post-state pair `(new_state, new_lane_tip)` the settlement advances the covenant to
/// with the L1 block (`block_prove_to`) the proof was committed against.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct SettlementInfo {
    /// L1 transaction id of the settlement.
    pub tx_id: TransactionId,
    /// L1 chain block that contained the settlement transaction.
    pub containing_block: Hash,
    /// DAA score of the containing block, as an unaligned little-endian `u64` so the struct stays
    /// `Unaligned`.
    pub daa_score: U64,
    /// L1 chain block hash the settlement proves up to.
    pub block_prove_to: Hash,
    /// L2 SMT state root after this settlement.
    pub new_state: [u8; 32],
    /// Lane tip after this settlement.
    pub new_lane_tip: Hash,
    /// Redeem-script hash committed by the settlement's continuation output (output 0), i.e. the
    /// P2SH SPK the next spend must reproduce. Carried from the observed transaction rather than
    /// rebuilt from the adopter's local redeem pins: a covenant pins its guest-ELF image ids at
    /// bootstrap, and a prover whose ELFs differ must fail adoption checks loudly instead of
    /// building a settlement the node's P2SH hash check rejects.
    pub continuation_spk_hash: [u8; 32],
    /// P2SH script-hash committed by the settlement's permission (exit) output (output 1), or
    /// `[0u8; 32]` when this settlement emitted no exits (single-output layout). The aggregate
    /// journal commits the same value (see `PermissionTreeAccumulator::finalize`).
    pub permission_spk_hash: [u8; 32],
}

// Borsh is hand-rolled because the zerocopy `daa_score: U64` wrapper carries no Borsh impl; it is
// serialized as a plain little-endian `u64`. Every other field defers to its own derived impl.
impl BorshSerialize for SettlementInfo {
    fn serialize<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        self.tx_id.serialize(writer)?;
        self.containing_block.serialize(writer)?;
        self.daa_score.get().serialize(writer)?;
        self.block_prove_to.serialize(writer)?;
        self.new_state.serialize(writer)?;
        self.new_lane_tip.serialize(writer)?;
        self.continuation_spk_hash.serialize(writer)?;
        self.permission_spk_hash.serialize(writer)
    }
}

impl BorshDeserialize for SettlementInfo {
    fn deserialize_reader<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        Ok(Self {
            tx_id: TransactionId::deserialize_reader(reader)?,
            containing_block: Hash::deserialize_reader(reader)?,
            daa_score: U64::new(u64::deserialize_reader(reader)?),
            block_prove_to: Hash::deserialize_reader(reader)?,
            new_state: <[u8; 32]>::deserialize_reader(reader)?,
            new_lane_tip: Hash::deserialize_reader(reader)?,
            continuation_spk_hash: <[u8; 32]>::deserialize_reader(reader)?,
            permission_spk_hash: <[u8; 32]>::deserialize_reader(reader)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the hand-rolled Borsh impls against field-order drift: distinct values per field so a
    /// serialize/deserialize mismatch would surface as an inequality.
    #[test]
    fn borsh_round_trip() {
        let info = SettlementInfo {
            tx_id: TransactionId::from_bytes([0x11; 32]),
            containing_block: Hash::from_bytes([0x22; 32]),
            daa_score: U64::new(0x0123_4567_89ab_cdef),
            block_prove_to: Hash::from_bytes([0x33; 32]),
            new_state: [0x44; 32],
            new_lane_tip: Hash::from_bytes([0x55; 32]),
            continuation_spk_hash: [0x66; 32],
            permission_spk_hash: [0x77; 32],
        };
        let bytes = borsh::to_vec(&info).expect("serialize");
        let decoded = SettlementInfo::try_from_slice(&bytes).expect("deserialize");
        assert_eq!(info, decoded);
    }
}
