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
//! - `control_id`   ← `SuccinctReceipt.control_id` digest bytes
//! - `hashfn`       ← string → u8 (`"blake2b"`=0, `"poseidon2"`=1, `"sha-256"`=2)
//! - `control_index`← `MerkleProof.index` (LE u32)
//! - `control_digests` ← concatenated `MerkleProof.digests`

use alloc::vec::Vec;

use risc0_zkvm::{Receipt, sha::Digestible};
use vprogs_zk_covenant::SuccinctWitness;

/// Owned byte buffers for the eight stack items the R0Succinct precompile pops, plus a
/// 1-byte zk tag handled by the covenant script. Construct via [`Self::from_receipt`] and
/// expose [`SuccinctWitness`] borrows via [`Self::as_witness`].
pub struct OwnedSuccinctWitness {
    seal: Vec<u8>,
    claim: [u8; 32],
    control_id: [u8; 32],
    hashfn: u8,
    control_index: u32,
    control_digests: Vec<u8>,
}

impl OwnedSuccinctWitness {
    /// Extracts the witness fields from a succinct receipt produced by the prover.
    ///
    /// Panics if the receipt isn't a succinct variant (we always prove with
    /// `ProverOpts::succinct()`), or if `hashfn` isn't a known kaspa-supported value. Both
    /// conditions are programmer errors at this layer.
    pub fn from_receipt(receipt: &Receipt) -> Self {
        let s = receipt.inner.succinct().expect("expected succinct receipt");

        let seal = s.seal.iter().flat_map(|w| w.to_le_bytes()).collect();
        let claim = <[u8; 32]>::from(s.claim.digest());
        let control_id = <[u8; 32]>::from(s.control_id);
        let hashfn = match s.hashfn.as_str() {
            "blake2b" => 0,
            "poseidon2" => 1,
            "sha-256" => 2,
            other => panic!("unsupported risc0 hashfn for kaspa precompile: {other:?}"),
        };
        let control_index = s.control_inclusion_proof.index;
        let control_digests = s
            .control_inclusion_proof
            .digests
            .iter()
            .flat_map(|d| <[u8; 32]>::from(*d))
            .collect();

        Self { seal, claim, control_id, hashfn, control_index, control_digests }
    }

    /// Borrows the owned bytes as a `SuccinctWitness` for `Settlement::build`.
    pub fn as_witness(&self) -> SuccinctWitness<'_> {
        SuccinctWitness {
            seal: &self.seal,
            claim: &self.claim,
            control_id: &self.control_id,
            hashfn: self.hashfn,
            control_index: self.control_index,
            control_digests: &self.control_digests,
        }
    }
}
