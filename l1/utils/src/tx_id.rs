use blake3::{Hasher, keyed_hash};

/// Computes the L1 v1 transaction ID from the raw `payload` field and the rest preimage.
///
/// Mirrors the Kaspa `id_v1` computation:
/// ```text
/// payload_digest = H_PayloadDigest(payload)
/// rest_digest    = H_TransactionRest(rest_preimage)
/// tx_id          = H_TransactionV1Id(payload_digest || rest_digest)
/// ```
///
/// Uses BLAKE3 keyed mode with the same domain separation keys as `kaspa-hashes`.
///
/// TODO: remove this once `kaspa-consensus-core` exposes a byte-oriented
/// `tx_id_from_parts(payload, rest_preimage)` helper usable from the zkVM guest.
pub fn tx_id_v1(payload: &[u8], rest_preimage: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new_keyed(&KEY_TRANSACTION_V1_ID);
    hasher.update(keyed_hash(&KEY_PAYLOAD_DIGEST, payload).as_bytes());
    hasher.update(keyed_hash(&KEY_TRANSACTION_REST, rest_preimage).as_bytes());
    *hasher.finalize().as_bytes()
}

/// BLAKE3 keys for each domain, zero-padded to 32 bytes. The fixed-size byte-string literal makes
/// the length check a compile-time type check.
const KEY_PAYLOAD_DIGEST: [u8; 32] = *b"PayloadDigest\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
const KEY_TRANSACTION_REST: [u8; 32] = *b"TransactionRest\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
const KEY_TRANSACTION_V1_ID: [u8; 32] = *b"TransactionV1Id\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
