use super::SuccinctWitness;

/// ZK witness produced by the host and consumed by the covenant's sig_script. The variant
/// must match the [`RedeemPins`](crate::script::RedeemPins) variant; `Settlement::build` panics
/// on mismatch.
pub enum SettlementWitness<'a> {
    /// Witness for a R0Succinct receipt: STARK seal + claim + control-inclusion proof.
    Succinct(SuccinctWitness<'a>),
    /// Witness for a Groth16 receipt: compressed proof bytes (the only thing the on-stack
    /// Groth16 verifier needs from the receipt; everything else is recomputed in-script from
    /// the journal hash + program id).
    Groth16 { compressed_proof: &'a [u8] },
}
