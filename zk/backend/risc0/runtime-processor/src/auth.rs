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
use signature::hazmat::PrehashVerifier;
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
/// upstream by the runtime (`SHA-256(domain || rest_preimage || payload_presig)`).
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
    vk.verify_prehash(message, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    /// Builds a minimal V1 `rest_preimage` whose input 0 spends `prev_tx_id`:0.
    /// `parse_input_outpoint_at` reads `version(2) || n_inputs(8) ||
    /// prev_tx_id(32) || prev_index(4)` and returns at that point, so the
    /// remaining transaction fields are not needed to name the outpoint.
    fn rest_preimage_with_one_input(prev_tx_id: &[u8; 32]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(prev_tx_id);
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    /// Both the witness preimage and the witness payload digest reach this
    /// function from `ctx.payload_bytes`, the user-authored L1 tx payload, via
    /// offsets that are themselves user-authored. A witness that does not hash
    /// to the named outpoint is therefore malformed user input, and the
    /// contract for malformed user input is `Err`: the guest must not panic on
    /// bytes an L1 transaction chooses.
    #[test]
    #[ignore = "repro: recover_prev_tx_v1_p2pk_pubkey asserts instead of returning Err; un-ignore with the fix"]
    fn witness_not_matching_outpoint_returns_err_rather_than_panicking() {
        let current_rest_preimage = rest_preimage_with_one_input(&[0xAA; 32]);
        let witness_rest_preimage = b"witness bytes that do not hash to the named outpoint";
        let witness_payload_digest = [0u8; 32];

        let result = recover_prev_tx_v1_p2pk_pubkey(
            &current_rest_preimage,
            0,
            witness_rest_preimage,
            &witness_payload_digest,
        );

        assert!(result.is_err(), "mismatched prev-tx witness must be rejected with Err");
    }
}
