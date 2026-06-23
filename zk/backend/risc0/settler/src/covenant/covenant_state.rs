use kaspa_consensus_core::tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry};
use kaspa_hashes::Hash;

/// The live on-chain covenant: identity, committed state, and the UTXO that carries it. Advanced by
/// each confirmed settlement.
#[derive(Clone)]
pub struct CovenantState {
    /// Consensus covenant id this UTXO is bound to.
    pub covenant_id: Hash,
    /// L2 SMT state root committed by the covenant's redeem prefix.
    pub state: [u8; 32],
    /// Lane tip committed by the covenant's redeem prefix.
    pub lane_tip: Hash,
    /// Outpoint of the UTXO carrying the covenant.
    pub outpoint: TransactionOutpoint,
    /// P2SH SPK of the carrying UTXO (its redeem script's hash).
    pub spk: ScriptPublicKey,
    /// Sompi locked in the carrying UTXO.
    pub value: u64,
    /// DAA score of the carrying UTXO, filled in once it confirms (needed to spend it next).
    pub daa_score: u64,
}

impl CovenantState {
    /// The [`UtxoEntry`] of the covenant's carrying outpoint, as a settlement spends it: the locked
    /// `value` under the covenant `spk`, at the confirmed `daa_score`, tagged with `covenant_id`.
    pub fn utxo_entry(&self) -> UtxoEntry {
        UtxoEntry::new(self.value, self.spk.clone(), self.daa_score, false, Some(self.covenant_id))
    }
}
