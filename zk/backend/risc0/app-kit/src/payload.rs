//! Assembly of runtime lane payloads across access metadata, signers, actions, and tail blocks.
//!
//! Implements the two-pass layout algorithm that probes the signers section with
//! zero offsets to determine section lengths, calculates concrete tail offsets,
//! and patches signer entries before producing the finalized payload bytes.

use vprogs_core_types::AccessMetadata;
use vprogs_l1_wallet::encode_activity_payload;
use vprogs_zk_backend_risc0_runtime_processor::runtime::compute_sig_message;

use crate::signer::{SignerKind, SignerSpec, TailBlock, encode_signer_entry};

/// Host-side assembly of one lane payload over the runtime ix framing:
/// `payload.bytes = access_meta || signers_section || actions_section || tail`.
///
/// The caller supplies sorted `access` (strictly ascending by resource id, the
/// ABI-layer invariant), app-encoded `actions` (each already `tag || body`), and
/// `signers` (non-strict ascending by `resource_idx`, the `decode_ix` invariant;
/// within a resource, ascending by pubkey). The assembler computes every tail
/// offset two-pass: a probe with zero offsets sizes the signers section, whose
/// length is invariant to the offset values, then the real tail block offsets
/// are patched in.
#[derive(Clone, Debug, Default)]
pub struct LanePayload {
    /// Ordered resource access metadata declarations.
    access: Vec<AccessMetadata>,
    /// Encoded action bodies (`tag || body`).
    actions: Vec<Vec<u8>>,
    /// Signer specifications and their associated tail blocks.
    signers: Vec<SignerSpec>,
}

/// What [`LanePayload::finish`] hands the signing closure: the signer's position in
/// the signers list and the BIP-340 digest `compute_sig_message(rest, presig)`.
pub struct SigRequest<'a> {
    /// Zero-based position of the signer within the payload's signers list.
    pub signer: usize,
    /// The 32-byte BIP-340 signature message digest.
    pub digest: &'a [u8; 32],
}

impl LanePayload {
    /// Creates an empty lane payload builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one access entry. Callers must add them in ascending resource-id order.
    pub fn access(mut self, meta: AccessMetadata) -> Self {
        self.access.push(meta);
        self
    }

    /// Appends one app-encoded action body (`tag || body`), preserving order.
    pub fn action(mut self, body: Vec<u8>) -> Self {
        self.actions.push(body);
        self
    }

    /// Appends one signer. Callers must add them ascending by `resource_idx`
    /// (non-strict), and within a resource ascending by pubkey.
    pub fn signer(mut self, spec: SignerSpec) -> Self {
        self.signers.push(spec);
        self
    }

    /// Returns the pre-signature prefix `access_meta || signers_section || actions_section`.
    ///
    /// This corresponds to `payload.bytes[..end_of_actions]`, the byte slice a Schnorr
    /// signer commits to via [`compute_sig_message`].
    pub fn presig(&self) -> Vec<u8> {
        let access_meta = encode_activity_payload(&self.access, &[]);
        let actions_section = encode_actions_section(&self.actions);

        // Pass 1: probe signers section with zero offsets to measure its fixed size.
        let mut probe_bytes = Vec::new();
        probe_bytes.extend_from_slice(&(self.signers.len() as u32).to_le_bytes());
        for spec in &self.signers {
            let entry = encode_signer_entry(spec.resource_idx, &spec.kind, 0, (0, 0, 0));
            probe_bytes.extend_from_slice(&entry);
        }

        let presig_len = access_meta.len() + probe_bytes.len() + actions_section.len();

        // Pass 2: compute concrete tail offsets starting immediately after presig.
        let mut current_tail_offset = presig_len;
        let mut signers_section = Vec::with_capacity(probe_bytes.len());
        signers_section.extend_from_slice(&(self.signers.len() as u32).to_le_bytes());

        for spec in &self.signers {
            let entry = match (&spec.kind, &spec.tail) {
                (
                    SignerKind::SigPtr { .. } | SignerKind::MultisigSigPtr { .. },
                    TailBlock::Sig64,
                ) => {
                    let sig_offset = current_tail_offset as u32;
                    current_tail_offset += 64;
                    encode_signer_entry(spec.resource_idx, &spec.kind, sig_offset, (0, 0, 0))
                }
                (SignerKind::Witness { .. }, TailBlock::Witness { prev_rest_preimage, .. }) => {
                    let rp_off = current_tail_offset as u32;
                    let rp_len = prev_rest_preimage.len() as u32;
                    current_tail_offset += prev_rest_preimage.len();
                    let pd_off = current_tail_offset as u32;
                    current_tail_offset += 32;
                    encode_signer_entry(spec.resource_idx, &spec.kind, 0, (rp_off, rp_len, pd_off))
                }
                _ => panic!("mismatched SignerKind and TailBlock in SignerSpec"),
            };
            signers_section.extend_from_slice(&entry);
        }

        debug_assert_eq!(signers_section.len(), probe_bytes.len());

        let mut presig = Vec::with_capacity(presig_len);
        presig.extend_from_slice(&access_meta);
        presig.extend_from_slice(&signers_section);
        presig.extend_from_slice(&actions_section);
        debug_assert_eq!(presig.len(), presig_len);

        presig
    }

    /// Completes the payload: lays out the tail (one block per signer, in signer
    /// order), patches each entry's offsets to its block, and fills `Sig64` blocks
    /// by calling `sign` with `compute_sig_message(rest_preimage, presig)`.
    ///
    /// The resulting payload length is independent of `rest_preimage` contents.
    pub fn finish(
        &self,
        rest_preimage: &[u8],
        sign: &mut dyn FnMut(SigRequest<'_>) -> [u8; 64],
    ) -> Vec<u8> {
        let presig = self.presig();
        let digest = compute_sig_message(rest_preimage, &presig);

        let mut payload = presig;
        for (idx, spec) in self.signers.iter().enumerate() {
            match &spec.tail {
                TailBlock::Sig64 => {
                    let sig = sign(SigRequest { signer: idx, digest: &digest });
                    payload.extend_from_slice(&sig);
                }
                TailBlock::Witness { prev_rest_preimage, prev_payload_digest } => {
                    payload.extend_from_slice(prev_rest_preimage);
                    payload.extend_from_slice(prev_payload_digest);
                }
            }
        }
        payload
    }

    /// Completes signature-free payloads (such as deposits or witness-only transactions).
    ///
    /// Panics if any signer requires a signature slot ([`TailBlock::Sig64`]).
    pub fn finish_unsigned(&self) -> Vec<u8> {
        let presig = self.presig();

        let mut payload = presig;
        for spec in &self.signers {
            match &spec.tail {
                TailBlock::Sig64 => {
                    panic!(
                        "finish_unsigned called on payload with Sig64 tail blocks; use finish instead"
                    );
                }
                TailBlock::Witness { prev_rest_preimage, prev_payload_digest } => {
                    payload.extend_from_slice(prev_rest_preimage);
                    payload.extend_from_slice(prev_payload_digest);
                }
            }
        }
        payload
    }
}

/// Encodes an actions section with little-endian count prefix followed by bodies.
fn encode_actions_section(actions: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(actions.len() as u32).to_le_bytes());
    for a in actions {
        out.extend_from_slice(a);
    }
    out
}

#[cfg(test)]
mod tests {
    use vprogs_core_types::AccessType;
    use vprogs_zk_backend_risc0_runtime_processor::{
        ix::decode_ix,
        signer_trait::Signer,
        signer_variants::{
            GenesisSchnorrSigPtrSigner, MultisigSchnorrSigPtrSigner, PrevTxV1WitnessSigner,
            SchnorrSigPtrSigner,
        },
    };

    use super::*;
    use crate::signer::Bip340Signer;

    #[test]
    fn test_single_schnorr_payload_parity() {
        let signer = Bip340Signer::new();
        let resource_id0 = [0x11u8; 32];
        let resource_id1 = [0x22u8; 32];
        let action_body =
            vec![0x03, 0x00, 0x01, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let payload_builder = LanePayload::new()
            .access(AccessMetadata {
                resource_id: resource_id0.into(),
                access_type: AccessType::Write,
            })
            .access(AccessMetadata {
                resource_id: resource_id1.into(),
                access_type: AccessType::Write,
            })
            .action(action_body.clone())
            .signer(SignerSpec {
                resource_idx: 0,
                kind: SignerKind::SigPtr { tag: SchnorrSigPtrSigner::TAG },
                tail: TailBlock::Sig64,
            });

        let rest_preimage = [0x55u8; 64];
        let mut signed_sig = [0u8; 64];
        let finalized = payload_builder.finish(&rest_preimage, &mut |req| {
            assert_eq!(req.signer, 0);
            signed_sig = signer.sign_digest(req.digest);
            signed_sig
        });

        // Verify presig layout:
        // access_meta: 4 bytes count + 2 * (32 bytes id + 1 byte access_type) = 70 bytes
        // signers: 4 bytes count + (1 byte res_idx + 1 byte tag + 4 bytes sig_offset) = 10 bytes
        // actions: 4 bytes count + 12 bytes body = 16 bytes
        // presig_len = 70 + 10 + 16 = 96 bytes
        let presig = payload_builder.presig();
        assert_eq!(presig.len(), 96);

        // sig_offset in signers section should be 96
        let sig_offset = u32::from_le_bytes(presig[76..80].try_into().unwrap());
        assert_eq!(sig_offset, 96);

        // Total finalized payload length should be 96 + 64 = 160 bytes
        assert_eq!(finalized.len(), 160);
        assert_eq!(&finalized[..96], &presig[..]);

        // Tail contains the produced signature
        assert_eq!(&finalized[96..], &signed_sig[..]);

        // Digest verified against secp256k1 Schnorr
        let expected_digest = compute_sig_message(&rest_preimage, &presig);
        let msg = secp256k1::Message::from_digest(expected_digest);
        let xonly = secp256k1::XOnlyPublicKey::from_slice(&signer.pubkey()).unwrap();
        let sig = secp256k1::schnorr::Signature::from_slice(&signed_sig).unwrap();
        assert!(secp256k1::SECP256K1.verify_schnorr(&sig, &msg, &xonly).is_ok());

        // Validate that battery decode_ix decodes the payload ix data successfully
        let access_prefix_len = 70;
        let ix_data = &finalized[access_prefix_len..];
        let decoded = decode_ix(ix_data, 2).unwrap();
        assert_eq!(decoded.signers.len(), 1);
        assert_eq!(decoded.actions.len(), 1);
        assert_eq!(access_prefix_len + decoded.end_of_actions_in_ix, presig.len());
    }

    #[test]
    fn test_two_signers_two_actions_mixed_tail() {
        let signer1 = Bip340Signer::new();
        let res0 = [0x01u8; 32];
        let res1 = [0x02u8; 32];

        let prev_rest = vec![0xaa; 40];
        let prev_digest = [0xbb; 32];

        // Action 1: Transfer action from 0 to 1 with amount 16
        let action1 = vec![0x03, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        // Action 2: Transfer action from 1 to 0 with amount 32
        let action2 = vec![0x03, 0x01, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let payload_builder = LanePayload::new()
            .access(AccessMetadata { resource_id: res0.into(), access_type: AccessType::Write })
            .access(AccessMetadata { resource_id: res1.into(), access_type: AccessType::Read })
            .action(action1)
            .action(action2)
            .signer(SignerSpec {
                resource_idx: 0,
                kind: SignerKind::SigPtr { tag: SchnorrSigPtrSigner::TAG },
                tail: TailBlock::Sig64,
            })
            .signer(SignerSpec {
                resource_idx: 1,
                kind: SignerKind::Witness { tag: PrevTxV1WitnessSigner::TAG, input_idx: 2 },
                tail: TailBlock::Witness {
                    prev_rest_preimage: prev_rest.clone(),
                    prev_payload_digest: prev_digest,
                },
            });

        let presig = payload_builder.presig();
        let rest = [0x99u8; 20];
        let mut signed_sig = [0u8; 64];
        let finalized = payload_builder.finish(&rest, &mut |req| {
            assert_eq!(req.signer, 0);
            signed_sig = signer1.sign_digest(req.digest);
            signed_sig
        });

        // Tail layout:
        // presig_len -> 64 bytes (signer 0 Sig64) -> 40 bytes (signer 1 rp) -> 32 bytes (signer 1
        // pd)
        let presig_len = presig.len();
        assert_eq!(finalized.len(), presig_len + 64 + 40 + 32);

        // Signer 0 sig_offset points to presig_len
        // Signer 1 rp_offset points to presig_len + 64, rp_len = 40, pd_offset = presig_len + 64 +
        // 40
        let sig0_slice = &finalized[presig_len..presig_len + 64];
        let witness_rp_slice = &finalized[presig_len + 64..presig_len + 64 + 40];
        let witness_pd_slice = &finalized[presig_len + 64 + 40..];

        assert_eq!(witness_rp_slice, &prev_rest[..]);
        assert_eq!(witness_pd_slice, &prev_digest[..]);
        assert_eq!(sig0_slice, &signed_sig[..]);

        let digest = compute_sig_message(&rest, &presig);
        let msg = secp256k1::Message::from_digest(digest);
        let xonly = secp256k1::XOnlyPublicKey::from_slice(&signer1.pubkey()).unwrap();
        let sig = secp256k1::schnorr::Signature::from_slice(&signed_sig).unwrap();
        assert!(secp256k1::SECP256K1.verify_schnorr(&sig, &msg, &xonly).is_ok());

        // Validate that battery decode_ix decodes the payload ix data successfully
        let access_prefix_len = 4 + 2 * 33;
        let ix_data = &finalized[access_prefix_len..];
        let decoded = decode_ix(ix_data, 2).unwrap();
        assert_eq!(decoded.signers.len(), 2);
        assert_eq!(decoded.actions.len(), 2);
        assert_eq!(access_prefix_len + decoded.end_of_actions_in_ix, presig.len());
    }

    #[test]
    fn test_adversarial_n_signers_ordering_and_indices() {
        let signer_a = Bip340Signer::new();
        let signer_b = Bip340Signer::new();
        let signer_c = Bip340Signer::new();

        let res0 = [0x01u8; 32];
        let res1 = [0x02u8; 32];
        let res2 = [0x03u8; 32];

        let witness_rest1 = vec![0x11; 50];
        let witness_digest1 = [0x22; 32];
        let witness_rest2 = vec![0x33; 30];
        let witness_digest2 = [0x44; 32];

        // 5 signers on resources 0, 0, 1, 2, 2 (non-strict ascending order)
        let payload_builder = LanePayload::new()
            .access(AccessMetadata { resource_id: res0.into(), access_type: AccessType::Write })
            .access(AccessMetadata { resource_id: res1.into(), access_type: AccessType::Write })
            .access(AccessMetadata { resource_id: res2.into(), access_type: AccessType::Read })
            .action(vec![0x03, 0x00, 0x01, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
            .signer(SignerSpec {
                resource_idx: 0,
                kind: SignerKind::SigPtr { tag: SchnorrSigPtrSigner::TAG },
                tail: TailBlock::Sig64,
            })
            .signer(SignerSpec {
                resource_idx: 0,
                kind: SignerKind::MultisigSigPtr {
                    tag: MultisigSchnorrSigPtrSigner::TAG,
                    pubkey_idx: 1,
                },
                tail: TailBlock::Sig64,
            })
            .signer(SignerSpec {
                resource_idx: 1,
                kind: SignerKind::Witness { tag: PrevTxV1WitnessSigner::TAG, input_idx: 0 },
                tail: TailBlock::Witness {
                    prev_rest_preimage: witness_rest1.clone(),
                    prev_payload_digest: witness_digest1,
                },
            })
            .signer(SignerSpec {
                resource_idx: 2,
                kind: SignerKind::SigPtr { tag: GenesisSchnorrSigPtrSigner::TAG },
                tail: TailBlock::Sig64,
            })
            .signer(SignerSpec {
                resource_idx: 2,
                kind: SignerKind::Witness { tag: PrevTxV1WitnessSigner::TAG, input_idx: 1 },
                tail: TailBlock::Witness {
                    prev_rest_preimage: witness_rest2.clone(),
                    prev_payload_digest: witness_digest2,
                },
            });

        let mut seen_indices = Vec::new();
        let finalized = payload_builder.finish(&[0xaa; 32], &mut |req| {
            seen_indices.push(req.signer);
            match req.signer {
                0 => signer_a.sign_digest(req.digest),
                1 => signer_b.sign_digest(req.digest),
                3 => signer_c.sign_digest(req.digest),
                other => panic!("unexpected signer index {other}"),
            }
        });

        // 3 signature slots requested: signer index 0, 1, and 3
        assert_eq!(seen_indices, vec![0, 1, 3]);

        // decode_ix successfully parses all 5 signers in order
        let access_prefix_len = 4 + 3 * 33;
        let ix_data = &finalized[access_prefix_len..];
        let decoded = decode_ix(ix_data, 3).unwrap();
        assert_eq!(decoded.signers.len(), 5);
        assert_eq!(decoded.signers[0].0, 0);
        assert_eq!(decoded.signers[1].0, 0);
        assert_eq!(decoded.signers[2].0, 1);
        assert_eq!(decoded.signers[3].0, 2);
        assert_eq!(decoded.signers[4].0, 2);
    }

    #[test]
    fn test_finish_length_invariant_to_rest_preimage() {
        let signer = Bip340Signer::new();
        let payload_builder = LanePayload::new()
            .access(AccessMetadata {
                resource_id: [0x12u8; 32].into(),
                access_type: AccessType::Write,
            })
            .action(vec![0x01, 0x02, 0x03])
            .signer(SignerSpec {
                resource_idx: 0,
                kind: SignerKind::SigPtr { tag: SchnorrSigPtrSigner::TAG },
                tail: TailBlock::Sig64,
            });

        let out1 = payload_builder.finish(&[], &mut |req| signer.sign_digest(req.digest));
        let out2 = payload_builder.finish(&[0x11; 50], &mut |req| signer.sign_digest(req.digest));
        let out3 = payload_builder.finish(&[0x22; 256], &mut |req| signer.sign_digest(req.digest));

        assert_eq!(out1.len(), out2.len());
        assert_eq!(out2.len(), out3.len());
    }

    #[test]
    fn test_empty_signers_deposit_shape() {
        let user_id = [0xaa; 32];
        let config_id = [0xcc; 32];
        let deposit_action = vec![0x05, 0x00, 0x00, 0x00, 0x00, 0x00];

        let payload_builder = LanePayload::new()
            .access(AccessMetadata { resource_id: user_id.into(), access_type: AccessType::Write })
            .access(AccessMetadata { resource_id: config_id.into(), access_type: AccessType::Read })
            .action(deposit_action.clone());

        let payload = payload_builder.finish_unsigned();

        // access_meta: 4 bytes + 2 * 33 bytes = 70 bytes
        // signers: 4 bytes (0 count)
        // actions: 4 bytes (1 count) + 6 bytes body = 10 bytes
        // total: 70 + 4 + 10 = 84 bytes
        assert_eq!(payload.len(), 84);
        assert_eq!(&payload[70..74], &[0, 0, 0, 0]); // 0 signers
        assert_eq!(&payload[74..78], &[1, 0, 0, 0]); // 1 action
        assert_eq!(&payload[78..], &deposit_action[..]);
    }
}
