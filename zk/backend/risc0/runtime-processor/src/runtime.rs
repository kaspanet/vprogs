//! Top-level handler invoked by `Guest::process_transaction`.

use vprogs_zk_abi::{
    Error as AbiError, Result as AbiResult,
    transaction_processor::{Resource, Transaction},
};

use crate::{
    action::apply_action,
    auth_context::{AuthContext, SchnorrUnlocker},
    ix::{DecodedIx, decode_ix},
    signer::SignerEnum,
    signer_trait::{Signer, SignerResolveContext},
    signer_variants::{PrevTxV1WitnessSigner, SchnorrSigPtrSigner},
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
    let DecodedIx { signers, actions, end_of_actions_in_ix } = decode_ix(payload.ix_data)?;

    // Translate end_of_actions from ix-relative to payload-relative bytes.
    // payload.bytes = access_metadata_prefix || ix_data, so the prefix length
    // is `payload.bytes.len() - ix_data.len()`.
    let access_prefix_len = payload.bytes.len() - payload.ix_data.len();
    let end_of_actions_in_payload = access_prefix_len + end_of_actions_in_ix;
    let payload_presig = &payload.bytes[..end_of_actions_in_payload];
    let sig_msg = compute_sig_message(current_rest_preimage, payload_presig);

    // Resolve signers → AuthContext. Buckets are sorted by (resource_idx, pubkey)
    // post-collection so lock matchers can rely on monotonic ordering.
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
/// produce the exact same blake3 keyed-hash input.
fn compute_sig_message(rest_preimage: &[u8], payload_presig: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(&KEY_SIG_MSG_V1);
    hasher.update(rest_preimage);
    hasher.update(payload_presig);
    *hasher.finalize().as_bytes()
}

/// Walks parsed signers, calls `Signer::resolve` on each, pushes
/// `(resource_idx, Unlocker)` into the matching `AuthContext` bucket, and
/// finalises by sorting each bucket by `(resource_idx, pubkey)` so lock
/// matchers can use binary search / merge walks.
fn resolve_signers<'a>(
    signers: &[(u8, SignerEnum<'a>)],
    ctx: &SignerResolveContext<'a>,
) -> AbiResult<AuthContext> {
    let mut auth = AuthContext::default();

    for (resource_idx, signer) in signers {
        let unlocker: SchnorrUnlocker = match signer {
            SignerEnum::SchnorrSigPtr(s) => SchnorrSigPtrSigner::resolve(s, *resource_idx, ctx)?,
            SignerEnum::PrevTxV1Witness(s) => {
                PrevTxV1WitnessSigner::resolve(s, *resource_idx, ctx)?
            }
            SignerEnum::_Phantom(_) => unreachable!("phantom variant"),
        };
        auth.schnorr.push((*resource_idx, unlocker));
    }

    auth.schnorr.sort_unstable_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| a.1.pubkey.cmp(&b.1.pubkey))
    });

    Ok(auth)
}
