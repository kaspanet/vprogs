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
pub fn compute_tx_id(payload: &[u8], rest_preimage: &[u8]) -> [u8; 32] {
    let payload_digest = blake3::keyed_hash(&padded_key(b"PayloadDigest"), payload);
    let rest_digest = blake3::keyed_hash(&padded_key(b"TransactionRest"), rest_preimage);

    let mut hasher = blake3::Hasher::new_keyed(&padded_key(b"TransactionV1Id"));
    hasher.update(payload_digest.as_bytes());
    hasher.update(rest_digest.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Creates a 32-byte key by copying the input bytes and zero-padding the remainder.
const fn padded_key(src: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    let mut i = 0;
    while i < src.len() {
        key[i] = src[i];
        i += 1;
    }
    key
}
