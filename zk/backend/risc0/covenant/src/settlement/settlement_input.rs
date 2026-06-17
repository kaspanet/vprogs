use kaspa_consensus_core::tx::TransactionOutpoint;
use kaspa_hashes::Hash;

use super::SettlementWitness;
use crate::script::RedeemPins;

/// Inputs describing a single settlement step.
pub struct SettlementInput<'a> {
    /// Covenant id carried forward by the continuation output.
    pub covenant_id: Hash,
    /// Verifier-identity constants baked into the redeem script. The variant determines which
    /// `OpZkPrecompile` branch the script terminates in and must match `witness`.
    pub pins: RedeemPins<'a>,
    /// L2 SMT state root before this batch.
    pub prev_state: &'a [u8; 32],
    /// Lane tip embedded in the covenant UTXO's redeem prefix (carried from the previous
    /// settlement).
    pub prev_lane_tip: &'a Hash,
    /// L2 SMT state root after this batch.
    pub new_state: &'a [u8; 32],
    /// Lane tip after this batch (locks into the continuation UTXO's redeem prefix and feeds
    /// into the guest's `seq_commit` derivation - rewind-resistant).
    pub new_lane_tip: &'a Hash,
    /// L1 chain block whose seq commitment the covenant script anchors `new_seq_commit` to.
    pub block_prove_to: Hash,
    /// UTXO outpoint of the covenant being spent.
    pub prev_outpoint: TransactionOutpoint,
    /// Value carried on the covenant UTXO. Split between continuation and permission outputs
    /// when [`Self::permission_spk_hash`] is non-zero (see
    /// [`Settlement::build`](super::Settlement::build)).
    pub value: u64,
    /// Proof-system-tagged ZK witness bytes pushed onto the redeem-spending sig_script. Must
    /// match the variant of `pins`.
    pub witness: SettlementWitness<'a>,
    /// `blake2b(perm_redeem_script)` from the batch journal. Non-zero →
    /// [`Settlement::build`](super::Settlement::build) emits a second covenant-bound P2SH exit
    /// output of value `pins.common().permission_output_value`. `[0; 32]` → single continuation
    /// output (no exits in this batch).
    pub permission_spk_hash: &'a [u8; 32],
}
