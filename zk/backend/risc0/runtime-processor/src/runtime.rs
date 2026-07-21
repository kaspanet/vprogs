//! Core transaction handler, wrapped by `main` into the ABI `TransactionHandler`
//! shape passed to `process_transaction`.

use vprogs_zk_abi::{
    Result as AbiResult,
    transaction_processor::{Resource, Transaction},
    withdrawal::{DepositSink, ExitSink},
};
use vprogs_zk_backend_risc0_api::{Hasher, Sha256};

use crate::{
    action::{ApplyContext, apply_action},
    auth_context::{AuthContext, MultisigUnlocker},
    deposit_policy::DepositPolicy,
    domain::Domain,
    ix::{DecodedIx, decode_ix},
    signer::SignerEnum,
    signer_trait::{Signer, SignerResolveContext},
    signer_variants::{
        GenesisSchnorrSigPtrSigner, MultisigPrevTxV1WitnessSigner, MultisigSchnorrSigPtrSigner,
        PrevTxV1WitnessSigner, SchnorrSigPtrSigner,
    },
};

/// Verifies signers against resource locks and applies the decoded actions.
///
/// `main` adapts this into the ABI [`TransactionHandler`] shape. `exits` receives L2-to-L1 exits
/// emitted by `Withdraw` actions; `deposit` receives the deposit-address commitment written by a
/// `Deposit` action; `merge_idx` and `context_hash` are unused.
///
/// Generic over `P: DepositPolicy` so a different runtime can supply its own deposit rules in
/// `main.rs` without touching this file.
///
/// [`TransactionHandler`]: vprogs_zk_abi::transaction_processor::TransactionHandler
pub fn run<'a, P: DepositPolicy>(
    tx: &Transaction<'a>,
    resources: &mut [Resource<'a>],
    exits: &mut ExitSink,
    deposit: &mut DepositSink,
    policy: &P,
) -> AbiResult<()> {
    // The witness format is V1-specific; the ABI only decodes V1 transactions.
    let current_rest_preimage = tx.rest_preimage;
    let payload = &tx.payload;
    let DecodedIx { signers, actions, end_of_actions_in_ix } =
        decode_ix(payload.ix_data, resources.len())?;

    // Translate end_of_actions from ix-relative to payload-relative bytes.
    // payload.bytes = access_metadata_prefix || ix_data, so the prefix length
    // is `payload.bytes.len() - ix_data.len()`. The sig-message digest over
    // this presig slice is hashed lazily by `SignerResolveContext`, so a
    // witness-only tx never pays for it.
    let access_prefix_len = payload.bytes.len() - payload.ix_data.len();
    let end_of_actions_in_payload = access_prefix_len + end_of_actions_in_ix;
    let payload_presig = &payload.bytes[..end_of_actions_in_payload];

    // Resolve signers → AuthContext. Entries are pushed in wire order. The
    // wire format must already be `(resource_idx asc, pubkey asc within
    // resource)`; `decode_ix` enforces the resource_idx ordering, and the
    // lock matchers (`SchnorrLockView`, `MultisigLockView`) reject malformed
    // per-resource slices at match time rather than silently re-sorting.
    let auth_ctx = resolve_signers(
        &signers,
        &SignerResolveContext::new(payload.bytes, current_rest_preimage, payload_presig, resources),
    )?;

    let mut cx = ApplyContext::new(tx, resources, &auth_ctx, exits, deposit);

    for action in &actions {
        apply_action(action, &mut cx, policy)?;
    }

    Ok(())
}

/// `M = SHA-256(Domain::SigMessage || len(rest_preimage) || rest_preimage || len(payload_presig) ||
/// payload_presig)`, each `len` a little-endian `u32`: the 32-byte digest a BIP-340 schnorr signer
/// commits to. Off-chain signers must produce the exact same domain-separated input. Exposed
/// publicly so test harnesses sign against the same function the guest verifies (no drift).
///
/// Length-prefixing both halves makes the preimage injective: distinct `(rest_preimage,
/// payload_presig)` pairs cannot share a digest, so a signature over one split is never valid for a
/// shifted one.
pub fn compute_sig_message(rest_preimage: &[u8], payload_presig: &[u8]) -> [u8; 32] {
    // Guest `usize` is 32-bit, so neither cast can truncate.
    let rest_len = (rest_preimage.len() as u32).to_le_bytes();
    let presig_len = (payload_presig.len() as u32).to_le_bytes();

    Sha256::hash_parts_with_domain(
        &[Domain::SigMessage as u8],
        [&rest_len[..], rest_preimage, &presig_len[..], payload_presig],
    )
}

/// Walks parsed signers, calls `Signer::resolve` on each, and routes the
/// produced unlocker into the matching `AuthContext` bucket. Each signer
/// kind is statically tied to one bucket via its `Signer::Unlocker`.
///
/// **Order is preserved verbatim from the wire**: no post-sort, no dedup. The
/// wire signer list is required to already be `(resource_idx asc, pubkey asc
/// within resource)`. `decode_ix` enforces the outer (resource_idx) ordering;
/// the inner (pubkey) ordering is verified by the lock matchers when they
/// slice the per-resource bucket. A malformed bucket (duplicates, wrong
/// order, or extras for a single-key lock) is rejected by the matcher and
/// surfaces as `lock not satisfied`.
///
/// Multisig contributions are *aggregated* per `resource_idx`: each
/// multisig-flavoured signer contributes one pubkey to the
/// `MultisigUnlocker.pubkeys` Vec for its resource. Since `decode_ix`
/// guarantees signers are sorted by `resource_idx`, a contiguous run of
/// multisig signers for the same resource lands in the same aggregated
/// entry (no map lookup or sort).
fn resolve_signers<'a>(
    signers: &[(u8, SignerEnum)],
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
            SignerEnum::GenesisSchnorrSigPtr(s) => {
                let u = GenesisSchnorrSigPtrSigner::resolve(s, resource_idx, ctx)?;
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
        }
    }

    Ok(auth)
}

/// Appends `contrib`'s pubkeys into the multisig bucket entry for
/// `resource_idx`, creating the entry if absent. Wire signers are sorted by
/// `resource_idx` (decode_ix invariant), so the target entry, if present,
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

    // Sig-message digest

    #[test]
    fn sig_msg_is_sha256_with_domain_of_length_prefixed_halves() {
        // Cross-check: compute_sig_message(rp, pp) ==
        //   SHA-256(Domain::SigMessage || len32(rp) || rp || len32(pp) || pp).
        let rp = b"rest-preimage";
        let pp = b"payload-presig-bytes";

        let got = compute_sig_message(rp, pp);

        let want = Sha256::hash_with_domain(
            &[Domain::SigMessage as u8],
            [
                &(rp.len() as u32).to_le_bytes()[..],
                rp.as_slice(),
                &(pp.len() as u32).to_le_bytes()[..],
                pp.as_slice(),
            ]
            .concat(),
        );
        assert_eq!(got, want);
    }

    #[test]
    fn sig_msg_changes_with_either_input() {
        let base = compute_sig_message(b"a", b"b");
        assert_ne!(base, compute_sig_message(b"a-different", b"b"));
        assert_ne!(base, compute_sig_message(b"a", b"b-different"));
    }

    #[test]
    fn sig_msg_is_domain_separated_from_plain_sha256() {
        // Plain SHA-256 of the same bytes must not collide with the
        // domain-separated version: that's the whole point of the domain tag.
        let rp = b"x";
        let pp = b"y";
        let domained = compute_sig_message(rp, pp);
        let plain = Sha256::hash([rp.as_slice(), pp.as_slice()].concat());
        assert_ne!(domained, plain);
    }

    #[test]
    fn sig_msg_concat_order_matters() {
        // The two halves of the input are not commutative; `rp || pp` and
        // `pp || rp` produce different digests for non-equal inputs.
        let a = compute_sig_message(b"left", b"right");
        let b = compute_sig_message(b"right", b"left");
        assert_ne!(a, b);
    }

    #[test]
    fn sig_msg_distinguishes_shifted_splits() {
        // The length prefixes commit to (rp, pp) as two distinct fields. Without them both
        // pairs below hash `domain || "abc"`, and one signature would cover either split.
        assert_ne!(compute_sig_message(b"ab", b"c"), compute_sig_message(b"a", b"bc"));

        // The degenerate splits of the same concatenation are distinct too.
        assert_ne!(compute_sig_message(b"", b"abc"), compute_sig_message(b"abc", b""));
    }

    // Multisig aggregator

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
        // append calls; no sort. Matchers reject if not strict-asc.
        let mut bucket = Vec::new();
        for pk in [[0x03u8; 32], [0x01u8; 32], [0x02u8; 32]] {
            append_multisig_contrib(&mut bucket, 7, MultisigUnlocker { pubkeys: alloc::vec![pk] });
        }
        assert_eq!(bucket.len(), 1);
        assert_eq!(bucket[0].1.pubkeys, alloc::vec![[0x03u8; 32], [0x01u8; 32], [0x02u8; 32]]);
    }
}
