use super::SuccinctWitness;

/// ZK witness produced by the host and consumed by the covenant's sig_script. The variant
/// must match the [`RedeemPins`](crate::script::RedeemPins) variant; `Settlement::build` panics
/// on mismatch.
pub enum SettlementWitness<'a> {
    /// Witness for a R0Succinct receipt: STARK seal + claim + control-inclusion proof, plus the
    /// bundle's witnessed `deposit_spk_hash`.
    Succinct(SuccinctWitness<'a>),
    /// Witness for a Groth16 receipt: compressed proof bytes (the only proof field the on-stack
    /// Groth16 verifier needs from the receipt; everything else is recomputed in-script from the
    /// journal hash + program id) plus the bundle's witnessed `deposit_spk_hash`.
    Groth16 {
        /// Compressed Groth16 proof bytes.
        compressed_proof: &'a [u8],
        /// Bundle deposit-address commitment (`StateTransition::deposit_spk_hash`); see
        /// [`SuccinctWitness::deposit_spk_hash`](super::SuccinctWitness::deposit_spk_hash) for how
        /// the redeem script consumes it.
        deposit_spk_hash: &'a [u8; 32],
    },
}
