use borsh::{BorshDeserialize, BorshSerialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::{Hash, TransactionId};

/// Decoded view of a settlement transaction observed on L1.
///
/// Bundles the post-state pair `(new_state, new_lane_tip)` the settlement advances the covenant to
/// with the L1 block (`block_prove_to`) the proof was committed against.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[derive(BorshSerialize, BorshDeserialize)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct SettlementInfo {
    /// L1 transaction id of the settlement.
    pub tx_id: TransactionId,
    /// L1 chain block that contained the settlement transaction.
    pub containing_block: Hash,
    /// L1 chain block hash the settlement proves up to.
    pub block_prove_to: Hash,
    /// L2 SMT state root after this settlement.
    pub new_state: [u8; 32],
    /// Lane tip after this settlement.
    pub new_lane_tip: Hash,
}
