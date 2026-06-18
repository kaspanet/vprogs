use kaspa_consensus_core::tx::{ScriptPublicKey, TransactionOutpoint};
use kaspa_hashes::Hash;

use super::covenant_state::CovenantState;

/// The covenant state a built settlement advances to, deferred until the settlement's finalized tx
/// id and the DAA score of its continuation UTXO are known (those depend on caller-specific fee
/// funding and confirmation, which [`build_settlement`](super::build_settlement) does not do).
pub struct CovenantAdvance {
    pub(super) covenant_id: Hash,
    pub(super) new_state: [u8; 32],
    pub(super) new_lane_tip: Hash,
    pub(super) continuation_spk: ScriptPublicKey,
    pub(super) value: u64,
}

impl CovenantAdvance {
    /// P2SH SPK of the continuation output: the next covenant UTXO's script, used to locate it on
    /// chain while confirming.
    pub fn continuation_spk(&self) -> &ScriptPublicKey {
        &self.continuation_spk
    }

    /// The covenant advanced past this settlement, given the finalized tx's `txid` (the
    /// continuation UTXO is its output 0) and the DAA score that UTXO confirmed at. Pass
    /// `daa_score` 0 when the confirmation score is not yet known (stamp it on confirmation).
    pub fn apply(self, txid: Hash, daa_score: u64) -> CovenantState {
        CovenantState {
            covenant_id: self.covenant_id,
            state: self.new_state,
            lane_tip: self.new_lane_tip,
            outpoint: TransactionOutpoint::new(txid, 0),
            spk: self.continuation_spk,
            value: self.value,
            daa_score,
        }
    }
}
