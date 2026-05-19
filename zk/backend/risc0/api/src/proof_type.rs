//! Host-side proof-system selector.
//!
//! Sits above the kaspa-txscript `ZkTag` byte (which is the on-chain script ABI for the
//! `OpZkPrecompile` dispatch): host orchestration picks a [`ProofType`], the
//! [`crate::Backend`] uses it to select `ProverOpts::groth16()` vs
//! `ProverOpts::succinct()`, and the caller can pattern-match on it to build the matching
//! covenant `RedeemPins` variant.

/// Proof system a [`crate::Backend`] produces for batch settlements.
///
/// Inner transaction receipts are always succinct (they fold into the batch as succinct
/// assumptions); this only selects the *outer* batch receipt's kind.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProofType {
    /// Risc0 succinct STARK receipt.
    Succinct,
    /// Risc0 Groth16 (BN254) receipt.
    Groth16,
}
