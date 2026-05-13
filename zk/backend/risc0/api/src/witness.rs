//! Settlement-witness extraction from a RISC-0 receipt.
//!
//! The covenant script consumes the receipt as a flat sequence of byte fields pushed onto the
//! script stack (see `kaspa_txscript::zk_precompiles::risc0::R0SuccinctPrecompile::verify_zk`).
//! [`OwnedSuccinctWitness::from_receipt`] pulls those fields out of a `Receipt` in the byte
//! layout the precompile expects, so [`vprogs_zk_covenant::SuccinctWitness`] borrows of the
//! same buffer feed `Settlement::build` directly.
//!
//! Mapping reference (kaspa-side):
//! - `seal`         ← LE-encoded `Vec<u32>`
//! - `claim`        ← `Digestible::digest::<sha::Impl>()` of the receipt's claim
//! - `control_index`← `MerkleProof.index` (LE u32)
//! - `control_digests` ← concatenated `MerkleProof.digests`
//!
//! [`ScriptVerifierPins`] separately exposes the script-embedded verifier-identity constants
//! (`control_id`, `hashfn`) extracted from the same receipt - these are no longer pushed via
//! sig_script (the covenant redeem hardcodes them), but the host still needs to know what
//! values to bake in for the proofs it actually produces.

use alloc::vec::Vec;

use risc0_zkvm::{Receipt, sha::Digestible};
use vprogs_zk_covenant::SuccinctWitness;

/// Owned byte buffers for the stack items the R0Succinct precompile pops from sig_script.
/// Construct via [`Self::from_receipt`] and expose [`SuccinctWitness`] borrows via
/// [`Self::as_witness`].
pub struct OwnedSuccinctWitness {
    seal: Vec<u8>,
    claim: [u8; 32],
    control_index: u32,
    control_digests: Vec<u8>,
}

/// Script-embedded verifier-identity constants extracted from a succinct receipt. Feed these
/// into [`vprogs_zk_covenant::SettlementInput::control_id`] / `hashfn` (and the matching
/// fields on `BootstrapInput`) so the redeem script bakes the same values the prover used.
pub struct ScriptVerifierPins {
    pub control_id: [u8; 32],
    pub hashfn: u8,
}

impl OwnedSuccinctWitness {
    /// Extracts the witness fields from a succinct receipt produced by the prover.
    ///
    /// Panics if the receipt isn't a succinct variant (we always prove with
    /// `ProverOpts::succinct()`). That condition is a programmer error at this layer.
    pub fn from_receipt(receipt: &Receipt) -> Self {
        let s = receipt.inner.succinct().expect("expected succinct receipt");

        let seal = s.seal.iter().flat_map(|w| w.to_le_bytes()).collect();
        let claim = <[u8; 32]>::from(s.claim.digest());
        let control_index = s.control_inclusion_proof.index;
        let control_digests =
            s.control_inclusion_proof.digests.iter().flat_map(|d| <[u8; 32]>::from(*d)).collect();

        Self { seal, claim, control_index, control_digests }
    }

    /// Borrows the owned bytes as a `SuccinctWitness` for `Settlement::build`.
    pub fn as_witness(&self) -> SuccinctWitness<'_> {
        SuccinctWitness {
            seal: &self.seal,
            claim: &self.claim,
            control_index: self.control_index,
            control_digests: &self.control_digests,
        }
    }

    /// Extracts the verifier-identity pins (control_id, hashfn) from a succinct receipt.
    /// Centralizes the risc0-string → kaspa-precompile-byte mapping for `hashfn`. Panics if
    /// the receipt isn't succinct, or if `hashfn` is unknown to the kaspa precompile.
    pub fn script_pins_from_receipt(receipt: &Receipt) -> ScriptVerifierPins {
        let s = receipt.inner.succinct().expect("expected succinct receipt");
        let control_id = <[u8; 32]>::from(s.control_id);
        let hashfn = match s.hashfn.as_str() {
            "blake2b" => 0,
            "poseidon2" => 1,
            "sha-256" => 2,
            other => panic!("unsupported risc0 hashfn for kaspa precompile: {other:?}"),
        };
        ScriptVerifierPins { control_id, hashfn }
    }
}
