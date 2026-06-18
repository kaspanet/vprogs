//! Settlement-witness extraction from a RISC-0 receipt.
//!
//! The covenant script consumes the receipt as a flat sequence of byte fields pushed onto the
//! script stack. For R0Succinct, those are the seal/claim/control-inclusion-proof pieces of a
//! `SuccinctReceipt`; for Groth16, the only stack-pushed field is the compressed proof bytes
//! (everything else — receipt-claim hash, public inputs, verifying key, control-root halves
//! — is reconstructed in-script from build-time constants and the journal hash).
//!
//! Mapping reference (kaspa-side, succinct):
//! - `seal`         ← LE-encoded `Vec<u32>`
//! - `claim`        ← `Digestible::digest::<sha::Impl>()` of the receipt's claim
//! - `control_index`← `MerkleProof.index` (LE u32)
//! - `control_digests` ← concatenated `MerkleProof.digests`
//!
//! The script-embedded verifier-identity constants (`control_id`, `hashfn`) are
//! circuit-determined and live as build-time consts in
//! [`vprogs_zk_backend_risc0_covenant::succinct_consts`], not per-receipt extractions.

use alloc::vec::Vec;

use ark_bn254::{Bn254, G1Affine, G2Affine};
use ark_groth16::Proof;
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use risc0_zkvm::{Receipt, sha::Digestible};
use vprogs_zk_backend_risc0_covenant::{SettlementWitness, SuccinctWitness};

/// Owned byte buffers for the stack items the R0Succinct precompile pops from sig_script.
/// Construct via [`Self::from_receipt`] and feed into the settlement builder via
/// [`Self::as_witness`].
pub struct OwnedSuccinctWitness {
    seal: Vec<u8>,
    claim: [u8; 32],
    control_index: u32,
    control_digests: Vec<u8>,
}

impl OwnedSuccinctWitness {
    /// Extracts the witness fields from a succinct receipt produced by the prover.
    ///
    /// Panics if the receipt isn't a succinct variant (we always prove tx receipts with
    /// `ProverOpts::succinct()` and batch receipts with `ProverOpts::succinct()` when the
    /// backend's `settlement_proof_type` is `ProofType::Succinct`). That condition is a
    /// programmer error at this layer.
    pub fn from_receipt(receipt: &Receipt) -> Self {
        let s = receipt.inner.succinct().expect("expected succinct receipt");

        let seal = s.seal.iter().flat_map(|w| w.to_le_bytes()).collect();
        let claim = <[u8; 32]>::from(s.claim.digest());
        let control_index = s.control_inclusion_proof.index;
        let control_digests =
            s.control_inclusion_proof.digests.iter().flat_map(|d| <[u8; 32]>::from(*d)).collect();

        Self { seal, claim, control_index, control_digests }
    }

    /// Borrows the owned bytes as a [`SettlementWitness::Succinct`] for `Settlement::build`.
    pub fn as_witness(&self) -> SettlementWitness<'_> {
        SettlementWitness::Succinct(self.as_succinct_witness())
    }

    /// Same as [`Self::as_witness`] but returns the inner [`SuccinctWitness`] directly. Useful
    /// for callers that already know they're on the succinct path and want to avoid the enum
    /// hop.
    pub fn as_succinct_witness(&self) -> SuccinctWitness<'_> {
        SuccinctWitness {
            seal: &self.seal,
            claim: &self.claim,
            control_index: self.control_index,
            control_digests: &self.control_digests,
        }
    }
}

/// Owned compressed-proof bytes the Groth16 covenant variant consumes from sig_script.
/// Construct via [`Self::from_receipt`]; feed [`Self::as_witness`] into `Settlement::build`.
pub struct OwnedGroth16Witness {
    compressed_proof: Vec<u8>,
}

impl OwnedGroth16Witness {
    /// Extracts the witness from a Groth16 receipt. Reads the risc0 `Seal` (G1/G2 affine
    /// points) out of the receipt's groth16 inner, deserializes them into arkworks types,
    /// then reserializes the `Proof<Bn254>` in compressed form — the byte layout the
    /// in-script Groth16 verifier consumes.
    ///
    /// Panics if the receipt isn't a Groth16 variant. That's a host-side wire-up bug (the
    /// backend's `settlement_proof_type` and the receipt's actual proof system are out of
    /// sync).
    pub fn from_receipt(receipt: &Receipt) -> Self {
        let seal_bytes = receipt.inner.groth16().expect("expected groth16 receipt").seal.as_slice();
        let compressed_proof = seal_to_compressed_proof(seal_bytes);
        Self { compressed_proof }
    }

    /// Borrows the owned bytes as a [`SettlementWitness::Groth16`].
    pub fn as_witness(&self) -> SettlementWitness<'_> {
        SettlementWitness::Groth16 { compressed_proof: &self.compressed_proof }
    }
}

/// Decodes a risc0 Groth16 seal (the abi-encoded G1/G2 affine triplet at
/// `receipt.inner.groth16().seal`) into compressed arkworks proof bytes.
fn seal_to_compressed_proof(seal: &[u8]) -> Vec<u8> {
    let seal = risc0_groth16::Seal::decode(seal).expect("decode risc0 groth16 seal");
    let proof = Proof::<Bn254> {
        a: g1_from_bytes(&seal.a),
        b: g2_from_bytes(&seal.b),
        c: g1_from_bytes(&seal.c),
    };
    let mut compressed = Vec::new();
    proof.serialize_compressed(&mut compressed).expect("serialize compressed proof");
    compressed
}

/// Deserialize a G1 element from risc0's big-endian byte representation. Panics on malformed
/// input — the caller is a host extracting from a receipt we ourselves just produced, so the
/// shape is a structural invariant.
fn g1_from_bytes(elem: &[Vec<u8>]) -> G1Affine {
    assert_eq!(elem.len(), 2, "malformed risc0 groth16 G1 element");
    let bytes: Vec<u8> = elem[0].iter().rev().chain(elem[1].iter().rev()).cloned().collect();
    G1Affine::deserialize_uncompressed(&*bytes).expect("deserialize G1 affine")
}

/// Deserialize a G2 element from risc0's big-endian byte representation.
fn g2_from_bytes(elem: &[Vec<Vec<u8>>]) -> G2Affine {
    assert!(
        elem.len() == 2 && elem[0].len() == 2 && elem[1].len() == 2,
        "malformed risc0 groth16 G2 element",
    );
    let bytes: Vec<u8> = elem[0][1]
        .iter()
        .rev()
        .chain(elem[0][0].iter().rev())
        .chain(elem[1][1].iter().rev())
        .chain(elem[1][0].iter().rev())
        .cloned()
        .collect();
    G2Affine::deserialize_uncompressed(&*bytes).expect("deserialize G2 affine")
}
