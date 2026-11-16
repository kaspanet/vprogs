//! Signer specifications and cryptographic helpers for host-side lane payloads.
//!
//! Provides the [`SignerSpec`] descriptor, wire-tag constants from the runtime
//! processor battery, and [`Bip340Signer`] for producing BIP-340 Schnorr signatures.

use secp256k1::Keypair;
pub use vprogs_zk_backend_risc0_runtime_processor::{
    signer_trait::Signer,
    signer_variants::{
        GenesisSchnorrSigPtrSigner, MultisigPrevTxV1WitnessSigner, MultisigSchnorrSigPtrSigner,
        PrevTxV1WitnessSigner, SchnorrSigPtrSigner,
    },
};

/// The tail block a signer's pointers reference: a 64-byte BIP-340 signature the
/// caller produces, or a prev-tx witness (`rest_preimage || payload_digest(32)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailBlock {
    /// A 64-byte signature slot, filled by the signing closure in
    /// [`LanePayload::finish`](crate::payload::LanePayload::finish).
    Sig64,
    /// A witness block carried verbatim into the tail.
    Witness {
        /// The serialized `rest_preimage` of the funding transaction.
        prev_rest_preimage: Vec<u8>,
        /// The 32-byte payload digest of the funding transaction.
        prev_payload_digest: [u8; 32],
    },
}

/// One signer entry the assembler lays out: `resource_idx u8 || kind u8 || body`.
///
/// `tag` is a battery variant's `TAG`; an app-defined kind that shares a body
/// layout (such as a custom genesis sig-pointer) supplies its own tag byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignerKind {
    /// Body `sig_offset u32` pointing to a [`TailBlock::Sig64`].
    SigPtr {
        /// Wire tag for the signer variant.
        tag: u8,
    },
    /// Body `pubkey_idx u8 || sig_offset u32` pointing to a [`TailBlock::Sig64`].
    MultisigSigPtr {
        /// Wire tag for the signer variant.
        tag: u8,
        /// Index of the public key within the target multisig lock.
        pubkey_idx: u8,
    },
    /// Body `input_idx u8 || rp_off u32 || rp_len u32 || pd_off u32` pointing to a
    /// [`TailBlock::Witness`].
    Witness {
        /// Wire tag for the signer variant.
        tag: u8,
        /// Index of the carrier input that spends the authed output.
        input_idx: u8,
    },
}

/// A fully-described signer entry (kind and tail), minus the offsets the assembler computes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignerSpec {
    /// Index of the target resource in the transaction's access list.
    pub resource_idx: u8,
    /// The wire kind and parameters for this signer.
    pub kind: SignerKind,
    /// The tail data block referenced by this signer.
    pub tail: TailBlock,
}

/// A BIP-340 (secp256k1 Schnorr) keypair for signing lane payloads; the x-only
/// public key is what a `SchnorrLockView` carries.
#[derive(Clone)]
pub struct Bip340Signer {
    /// The underlying secp256k1 keypair.
    keypair: Keypair,
    /// 32-byte serialized x-only public key.
    pubkey: [u8; 32],
}

impl Bip340Signer {
    /// Creates a fresh random signer.
    pub fn new() -> Self {
        let keypair = Keypair::new(secp256k1::SECP256K1, &mut rand::thread_rng());
        let pubkey = keypair.x_only_public_key().0.serialize();
        Self { keypair, pubkey }
    }

    /// Creates a signer from an existing secret key.
    pub fn from_secret_key(sk: &secp256k1::SecretKey) -> Self {
        let keypair = Keypair::from_secret_key(secp256k1::SECP256K1, sk);
        let pubkey = keypair.x_only_public_key().0.serialize();
        Self { keypair, pubkey }
    }

    /// Returns the 32-byte x-only public key.
    pub fn pubkey(&self) -> [u8; 32] {
        self.pubkey
    }

    /// Signs a 32-byte digest, returning the 64-byte BIP-340 signature.
    pub fn sign_digest(&self, digest: &[u8; 32]) -> [u8; 64] {
        secp256k1::SECP256K1
            .sign_schnorr(&secp256k1::Message::from_digest(*digest), &self.keypair)
            .serialize()
    }
}

impl Default for Bip340Signer {
    fn default() -> Self {
        Self::new()
    }
}

/// Encodes one signer entry given its resolved offsets.
pub(crate) fn encode_signer_entry(
    resource_idx: u8,
    kind: &SignerKind,
    sig_offset: u32,
    witness_offsets: (u32, u32, u32),
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(resource_idx);
    match kind {
        SignerKind::SigPtr { tag } => {
            out.push(*tag);
            out.extend_from_slice(&sig_offset.to_le_bytes());
        }
        SignerKind::MultisigSigPtr { tag, pubkey_idx } => {
            out.push(*tag);
            out.push(*pubkey_idx);
            out.extend_from_slice(&sig_offset.to_le_bytes());
        }
        SignerKind::Witness { tag, input_idx } => {
            out.push(*tag);
            out.push(*input_idx);
            out.extend_from_slice(&witness_offsets.0.to_le_bytes());
            out.extend_from_slice(&witness_offsets.1.to_le_bytes());
            out.extend_from_slice(&witness_offsets.2.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bip340_signer_roundtrip() {
        let signer = Bip340Signer::new();
        let digest = [0x42u8; 32];
        let sig_bytes = signer.sign_digest(&digest);

        let msg = secp256k1::Message::from_digest(digest);
        let xonly = secp256k1::XOnlyPublicKey::from_slice(&signer.pubkey()).unwrap();
        let sig = secp256k1::schnorr::Signature::from_slice(&sig_bytes).unwrap();
        assert!(secp256k1::SECP256K1.verify_schnorr(&sig, &msg, &xonly).is_ok());
    }

    #[test]
    fn test_bip340_signer_from_secret_key() {
        let sk = secp256k1::SecretKey::from_slice(&[0x01; 32]).unwrap();
        let signer = Bip340Signer::from_secret_key(&sk);
        let expected_keypair = Keypair::from_secret_key(secp256k1::SECP256K1, &sk);
        assert_eq!(signer.pubkey(), expected_keypair.x_only_public_key().0.serialize());
    }

    #[test]
    fn test_signer_kind_wire_encoding_parity() {
        // SigPtr wire parity: resource_idx || TAG || sig_offset (LE u32)
        let sig_ptr_bytes = encode_signer_entry(
            3,
            &SignerKind::SigPtr { tag: SchnorrSigPtrSigner::TAG },
            0x12345678,
            (0, 0, 0),
        );
        let mut expected_sig_ptr = vec![3, SchnorrSigPtrSigner::TAG];
        expected_sig_ptr.extend_from_slice(&0x12345678u32.to_le_bytes());
        assert_eq!(sig_ptr_bytes, expected_sig_ptr);

        // Genesis SigPtr wire parity (tag 0x05)
        let gen_ptr_bytes = encode_signer_entry(
            0,
            &SignerKind::SigPtr { tag: GenesisSchnorrSigPtrSigner::TAG },
            0xaabbccdd,
            (0, 0, 0),
        );
        let mut expected_gen_ptr = vec![0, GenesisSchnorrSigPtrSigner::TAG];
        expected_gen_ptr.extend_from_slice(&0xaabbccddu32.to_le_bytes());
        assert_eq!(gen_ptr_bytes, expected_gen_ptr);

        // MultisigSigPtr wire parity: resource_idx || TAG || pubkey_idx || sig_offset (LE u32)
        let multi_sig_bytes = encode_signer_entry(
            2,
            &SignerKind::MultisigSigPtr { tag: MultisigSchnorrSigPtrSigner::TAG, pubkey_idx: 1 },
            0x20406080,
            (0, 0, 0),
        );
        let mut expected_multi = vec![2, MultisigSchnorrSigPtrSigner::TAG, 1];
        expected_multi.extend_from_slice(&0x20406080u32.to_le_bytes());
        assert_eq!(multi_sig_bytes, expected_multi);

        // Witness wire parity: resource_idx || TAG || input_idx || rp_off || rp_len || pd_off
        let witness_bytes = encode_signer_entry(
            1,
            &SignerKind::Witness { tag: PrevTxV1WitnessSigner::TAG, input_idx: 4 },
            0,
            (100, 200, 300),
        );
        let mut expected_witness = vec![1, PrevTxV1WitnessSigner::TAG, 4];
        expected_witness.extend_from_slice(&100u32.to_le_bytes());
        expected_witness.extend_from_slice(&200u32.to_le_bytes());
        expected_witness.extend_from_slice(&300u32.to_le_bytes());
        assert_eq!(witness_bytes, expected_witness);

        // Multisig witness wire parity (TAG 0x04)
        let multi_witness_bytes = encode_signer_entry(
            5,
            &SignerKind::Witness { tag: MultisigPrevTxV1WitnessSigner::TAG, input_idx: 0 },
            0,
            (50, 60, 70),
        );
        let mut expected_multi_witness = vec![5, MultisigPrevTxV1WitnessSigner::TAG, 0];
        expected_multi_witness.extend_from_slice(&50u32.to_le_bytes());
        expected_multi_witness.extend_from_slice(&60u32.to_le_bytes());
        expected_multi_witness.extend_from_slice(&70u32.to_le_bytes());
        assert_eq!(multi_witness_bytes, expected_multi_witness);
    }
}
