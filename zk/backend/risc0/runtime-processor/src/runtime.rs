//! Top-level handler invoked by `Guest::process_transaction`.

use vprogs_zk_abi::{
    Error as AbiError, Result as AbiResult,
    transaction_processor::{Resource, Transaction},
};

#[cfg(feature = "experimental-image-lock")]
use crate::signer_variants::ImageProofSigner;
use crate::{
    action::apply_action,
    auth_context::{AuthContext, MultisigUnlocker},
    ix::{DecodedIx, decode_ix},
    signer::SignerEnum,
    signer_trait::{Signer, SignerResolveContext},
    signer_variants::{
        MultisigPrevTxV1WitnessSigner, MultisigSchnorrSigPtrSigner, PrevTxV1WitnessSigner,
        SchnorrSigPtrSigner,
    },
};

/// 32-byte BLAKE3 keyed-hash key for the schnorr message digest. Same shape
/// as the kaspa-hashes domain keys (`vprogs-l1-utils::tx_id_v1`): zero-padded
/// ASCII tag, length 32. Distinct from any tx_id key so an attacker can't
/// replay a tx_id digest as a runtime sig.
pub const KEY_SIG_MSG_V1: [u8; 32] = *b"RuntimeSigMessageV1\0\0\0\0\0\0\0\0\0\0\0\0\0";

pub fn run<'a>(
    tx: &Transaction<'a>,
    _tx_index: u32,
    _context_hash: &'a [u8; 32],
    resources: &mut [Resource<'a>],
) -> AbiResult<()> {
    // Only V1 transactions are supported (the witness format is V1-specific).
    let current_rest_preimage = match tx {
        Transaction::V1 { rest_preimage, .. } => *rest_preimage,
        Transaction::V0 { .. } => {
            return Err(AbiError::Decode("runtime: V0 transactions not supported".into()));
        }
    };

    let payload = tx.payload();
    let DecodedIx { signers, actions, end_of_actions_in_ix } =
        decode_ix(payload.ix_data, resources.len())?;

    // Translate end_of_actions from ix-relative to payload-relative bytes.
    // payload.bytes = access_metadata_prefix || ix_data, so the prefix length
    // is `payload.bytes.len() - ix_data.len()`.
    let access_prefix_len = payload.bytes.len() - payload.ix_data.len();
    let end_of_actions_in_payload = access_prefix_len + end_of_actions_in_ix;
    let payload_presig = &payload.bytes[..end_of_actions_in_payload];
    let sig_msg = compute_sig_message(current_rest_preimage, payload_presig);

    // Resolve signers → AuthContext. Entries are pushed in wire order. The
    // wire format must already be `(resource_idx asc, pubkey asc within
    // resource)` — `decode_ix` enforces the resource_idx ordering, and the
    // lock matchers (`SchnorrLockView`, `MultisigLockView`) reject malformed
    // per-resource slices at match time rather than silently re-sorting.
    let auth_ctx = resolve_signers(
        &signers,
        &SignerResolveContext {
            payload_bytes: payload.bytes,
            current_rest_preimage,
            sig_msg: &sig_msg,
            resources,
        },
    )?;

    for action in &actions {
        apply_action(action, tx, resources, &auth_ctx)?;
    }

    Ok(())
}

/// `M = blake3_keyed(KEY_SIG_MSG_V1, rest_preimage || payload_presig)` — the
/// 32-byte digest a BIP-340 schnorr signer commits to. Off-chain signers must
/// produce the exact same blake3 keyed-hash input. Exposed publicly so test
/// harnesses sign against the same function the guest verifies (no drift).
pub fn compute_sig_message(rest_preimage: &[u8], payload_presig: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(&KEY_SIG_MSG_V1);
    hasher.update(rest_preimage);
    hasher.update(payload_presig);
    *hasher.finalize().as_bytes()
}

/// Walks parsed signers, calls `Signer::resolve` on each, and routes the
/// produced unlocker into the matching `AuthContext` bucket. Each signer
/// kind is statically tied to one bucket via its `Signer::Unlocker`.
///
/// **Order is preserved verbatim from the wire**: no post-sort, no dedup. The
/// wire signer list is required to already be `(resource_idx asc, pubkey asc
/// within resource)`. `decode_ix` enforces the outer (resource_idx) ordering;
/// the inner (pubkey) ordering is verified by the lock matchers when they
/// slice the per-resource bucket. A malformed bucket — duplicates, wrong
/// order, or extras for a single-key lock — is rejected by the matcher and
/// surfaces as `lock not satisfied`.
///
/// Multisig contributions are *aggregated* per `resource_idx`: each
/// multisig-flavoured signer contributes one pubkey to the
/// `MultisigUnlocker.pubkeys` Vec for its resource. Since `decode_ix`
/// guarantees signers are sorted by `resource_idx`, a contiguous run of
/// multisig signers for the same resource lands in the same aggregated
/// entry — no map lookup or sort.
fn resolve_signers<'a>(
    signers: &[(u8, SignerEnum<'a>)],
    ctx: &SignerResolveContext<'a>,
) -> AbiResult<AuthContext> {
    let mut auth = AuthContext::default();

    for (resource_idx, signer) in signers {
        let resource_idx = *resource_idx;
        match signer {
            SignerEnum::SchnorrSigPtr(s) => {
                let u = SchnorrSigPtrSigner::resolve(s, resource_idx, ctx)?;
                auth.schnorr.push((resource_idx, u));
            }
            SignerEnum::PrevTxV1Witness(s) => {
                let u = PrevTxV1WitnessSigner::resolve(s, resource_idx, ctx)?;
                auth.schnorr.push((resource_idx, u));
            }
            SignerEnum::MultisigSchnorrSigPtr(s) => {
                let u = MultisigSchnorrSigPtrSigner::resolve(s, resource_idx, ctx)?;
                append_multisig_contrib(&mut auth.multisig, resource_idx, u);
            }
            SignerEnum::MultisigPrevTxV1Witness(s) => {
                let u = MultisigPrevTxV1WitnessSigner::resolve(s, resource_idx, ctx)?;
                append_multisig_contrib(&mut auth.multisig, resource_idx, u);
            }
            #[cfg(feature = "experimental-image-lock")]
            SignerEnum::ImageProof(s) => {
                let u = ImageProofSigner::resolve(s, resource_idx, ctx)?;
                auth.preimage.push((resource_idx, u));
            }
            SignerEnum::_Phantom(_) => unreachable!("phantom variant"),
        }
    }

    Ok(auth)
}

/// Appends `contrib`'s pubkeys into the multisig bucket entry for
/// `resource_idx`, creating the entry if absent. Wire signers are sorted by
/// `resource_idx` (decode_ix invariant), so the target entry — if present —
/// is always the last one in the bucket; this avoids any map lookup.
pub(crate) fn append_multisig_contrib(
    bucket: &mut alloc::vec::Vec<(u8, MultisigUnlocker)>,
    resource_idx: u8,
    contrib: MultisigUnlocker,
) {
    if let Some((last_idx, last)) = bucket.last_mut() {
        if *last_idx == resource_idx {
            last.pubkeys.extend(contrib.pubkeys);
            return;
        }
    }
    bucket.push((resource_idx, contrib));
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    // —— Sig-message digest ————————————————————————————————————————————————

    #[test]
    fn sig_msg_is_keyed_blake3_of_concat() {
        // Cross-check: compute_sig_message(rp, pp) ==
        //   blake3_keyed(KEY_SIG_MSG_V1, rp || pp).
        let rp = b"rest-preimage";
        let pp = b"payload-presig-bytes";

        let got = compute_sig_message(rp, pp);

        let mut concat = Vec::new();
        concat.extend_from_slice(rp);
        concat.extend_from_slice(pp);
        let want = blake3::Hasher::new_keyed(&KEY_SIG_MSG_V1).update(&concat).finalize();
        assert_eq!(&got, want.as_bytes());
    }

    #[test]
    fn sig_msg_changes_with_either_input() {
        let base = compute_sig_message(b"a", b"b");
        assert_ne!(base, compute_sig_message(b"a-different", b"b"));
        assert_ne!(base, compute_sig_message(b"a", b"b-different"));
    }

    #[test]
    fn sig_msg_is_domain_separated_from_unkeyed_blake3() {
        // Plain blake3 of the same bytes must not collide with the keyed
        // version: that's the whole point of the domain key.
        let rp = b"x";
        let pp = b"y";
        let keyed = compute_sig_message(rp, pp);
        let unkeyed = blake3::hash(&[rp.as_slice(), pp.as_slice()].concat());
        assert_ne!(&keyed, unkeyed.as_bytes());
    }

    #[test]
    fn sig_msg_concat_order_matters() {
        // The two halves of the input are not commutative — `rp || pp` and
        // `pp || rp` produce different digests for non-equal inputs.
        let a = compute_sig_message(b"left", b"right");
        let b = compute_sig_message(b"right", b"left");
        assert_ne!(a, b);
    }

    #[test]
    fn sig_msg_split_invariance() {
        // Splitting the second argument across two calls is forbidden — the
        // function commits to (rp, pp) as two distinct fields. This is what
        // lets the test harness keep `rest_preimage` and `payload_presig`
        // separate without an ambiguity-prone glue byte.
        let rp = b"r";
        let split = compute_sig_message(rp, b"hello-world");
        // Hand-rolled equivalent: blake3_keyed(K, rp || pp).
        let combined = {
            let mut h = blake3::Hasher::new_keyed(&KEY_SIG_MSG_V1);
            h.update(rp);
            h.update(b"hello-world");
            *h.finalize().as_bytes()
        };
        assert_eq!(split, combined);
    }

    // —— Multisig aggregator ————————————————————————————————————————————

    #[test]
    fn multisig_aggregator_appends_to_existing_entry_for_same_resource() {
        let mut bucket = Vec::new();
        append_multisig_contrib(
            &mut bucket,
            0,
            MultisigUnlocker { pubkeys: alloc::vec![[0x01u8; 32]] },
        );
        append_multisig_contrib(
            &mut bucket,
            0,
            MultisigUnlocker { pubkeys: alloc::vec![[0x02u8; 32]] },
        );
        assert_eq!(bucket.len(), 1);
        assert_eq!(bucket[0].0, 0);
        assert_eq!(bucket[0].1.pubkeys, alloc::vec![[0x01u8; 32], [0x02u8; 32]]);
    }

    #[test]
    fn multisig_aggregator_creates_new_entry_for_different_resource() {
        let mut bucket = Vec::new();
        append_multisig_contrib(
            &mut bucket,
            0,
            MultisigUnlocker { pubkeys: alloc::vec![[0x01u8; 32]] },
        );
        append_multisig_contrib(
            &mut bucket,
            1,
            MultisigUnlocker { pubkeys: alloc::vec![[0x02u8; 32]] },
        );
        assert_eq!(bucket.len(), 2);
        assert_eq!(bucket[0].0, 0);
        assert_eq!(bucket[1].0, 1);
    }

    #[test]
    fn multisig_aggregator_preserves_wire_order_within_resource() {
        // Order of pubkeys in the aggregated list mirrors the order of
        // append calls — no sort. Matchers reject if not strict-asc.
        let mut bucket = Vec::new();
        for pk in [[0x03u8; 32], [0x01u8; 32], [0x02u8; 32]] {
            append_multisig_contrib(&mut bucket, 7, MultisigUnlocker { pubkeys: alloc::vec![pk] });
        }
        assert_eq!(bucket.len(), 1);
        assert_eq!(bucket[0].1.pubkeys, alloc::vec![[0x03u8; 32], [0x01u8; 32], [0x02u8; 32]]);
    }

    #[test]
    fn key_const_is_32_zero_padded_ascii() {
        // The key is intentionally an ASCII tag zero-padded to 32 bytes so
        // it's grep-able and matches the kaspa-hashes domain-key style.
        assert_eq!(KEY_SIG_MSG_V1.len(), 32);
        assert!(KEY_SIG_MSG_V1.starts_with(b"RuntimeSigMessageV1"));
        // No non-zero bytes after the tag.
        assert!(KEY_SIG_MSG_V1[b"RuntimeSigMessageV1".len()..].iter().all(|b| *b == 0));
    }
}
