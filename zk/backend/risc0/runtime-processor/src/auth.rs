//! Witness verification helpers used by `runtime::resolve_sigs`.
//!
//! Two flavors:
//! - `recover_prev_tx_v1_p2pk_pubkey`: turns a `PrevTxV1Witness` into the P2PK pubkey from the
//!   prev-tx output that the named current-tx input spends. The runtime then matches this against
//!   the resource's lock.
//! - `verify_k256_schnorr_sig`: verifies a BIP-340 schnorr signature against a pubkey already named
//!   by the resource's lock.
//!
//! Cheat detection (host substitution attempts → proof aborts):
//! - `recover_prev_tx_v1_p2pk_pubkey` `assert_eq!`s the witness preimage against the outpoint's
//!   `prev_tx_id`.
//!
//! User-error cases (return `Ok(None)` / `false`, runtime errors out):
//! - prev output not P2PK; output index out of range.
//! - schnorr signature invalid.

use k256::schnorr::{Signature, VerifyingKey};
use signature::Verifier;
use vprogs_core_codec::Result as CodecResult;
use vprogs_l1_utils::tx_id_v1_from_digest;

use crate::tx_inputs::{parse_input_outpoint_at, parse_output_at_index_v1};

/// P2PK SPK opcode constants (Kaspa script subset).
const OP_DATA_32: u8 = 0x20;
const OP_CHECK_SIG: u8 = 0xac;
const P2PK_SPK_SIZE: usize = 34;

/// Recovers the P2PK pubkey authorizing the spend of input `input_idx` of the
/// current transaction.
///
/// Returns:
/// - `Ok(Some(pubkey))` if the witness verifies and the prev output is P2PK.
/// - `Ok(None)` if input_idx is in range but the prev output isn't P2PK (user error; runtime will
///   treat as no auth).
/// - `Err(_)` if the current tx's input list is too short, or if the witness preimage is malformed.
///
/// Panics on host cheating (witness doesn't hash to claimed outpoint).
pub fn recover_prev_tx_v1_p2pk_pubkey(
    current_tx_rest_preimage: &[u8],
    input_idx: u8,
    witness_rest_preimage: &[u8],
    witness_payload_digest: &[u8; 32],
) -> CodecResult<Option<[u8; 32]>> {
    // Step 1: Outpoint (prev_tx_id, output_index) of the spending input.
    let (expected_prev_tx_id, output_index) =
        parse_input_outpoint_at(current_tx_rest_preimage, input_idx as u32)?;

    // Step 2: ASSERT witness hashes to that outpoint's prev_tx_id.
    let computed_prev_tx_id = tx_id_v1_from_digest(witness_payload_digest, witness_rest_preimage);
    assert_eq!(
        &computed_prev_tx_id, expected_prev_tx_id,
        "host cheating: prev_tx witness does not match outpoint"
    );

    // Step 3: Extract the spent output's SPK; if not P2PK, no pubkey to recover.
    let output = parse_output_at_index_v1(witness_rest_preimage, output_index)?;
    Ok(extract_p2pk_pubkey(output.spk))
}

/// Extracts the 32-byte X-only pubkey from a 34-byte Kaspa P2PK SPK.
/// Format: `OpData32 (0x20) || pubkey[32] || OpCheckSig (0xac)`.
pub fn extract_p2pk_pubkey(spk: &[u8]) -> Option<[u8; 32]> {
    if spk.len() != P2PK_SPK_SIZE || spk[0] != OP_DATA_32 || spk[33] != OP_CHECK_SIG {
        return None;
    }
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&spk[1..33]);
    Some(pk)
}

/// Verifies a 64-byte BIP-340 schnorr signature against a 32-byte X-only pubkey
/// over the given message. Pure-function: the message digest is constructed
/// upstream by the runtime (`blake3(domain || rest_preimage || payload_presig)`).
///
/// Returns `false` for any failure (bad pubkey encoding, bad sig encoding,
/// signature equation doesn't hold). Treated as user error by the caller.
pub fn verify_k256_schnorr_sig(
    pubkey: &[u8; 32],
    signature: &[u8; 64],
    message: &[u8; 32],
) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pubkey) else { return false };
    let Ok(sig) = Signature::try_from(&signature[..]) else { return false };
    vk.verify(message, &sig).is_ok()
}
