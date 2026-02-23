//! Blake3 domain-separated hashing utilities.

/// Pad a domain separator into a 32-byte blake3 key.
///
/// Shorter domains are zero-padded on the right.
pub const fn domain_to_key(domain: &[u8]) -> [u8; blake3::KEY_LEN] {
    let mut key = [0u8; blake3::KEY_LEN];
    let mut i = 0usize;
    while i < domain.len() {
        key[i] = domain[i];
        i += 1;
    }
    key
}

/// Blake3 keyed hash of arbitrary data under `domain`.
#[inline]
pub fn domain_hash(domain: &[u8], data: &[u8]) -> [u8; 32] {
    let key = domain_to_key(domain);
    *blake3::keyed_hash(&key, data).as_bytes()
}

/// Blake3 keyed hash of two 32-byte inputs under `domain`.
#[inline]
pub fn domain_hash_pair(domain: &[u8], left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let key = domain_to_key(domain);
    let mut hasher = blake3::Hasher::new_keyed(&key);
    hasher.update(left);
    hasher.update(right);
    *hasher.finalize().as_bytes()
}

/// Compute `blake3(data)` (unkeyed).
#[inline]
pub fn state_hash(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_to_key_padding() {
        let key = domain_to_key(b"Hello");
        assert_eq!(&key[..5], b"Hello");
        assert!(key[5..].iter().all(|&b| b == 0));
    }

    #[test]
    fn domain_hash_deterministic() {
        let a = domain_hash(b"Test", b"data");
        let b = domain_hash(b"Test", b"data");
        assert_eq!(a, b);
    }

    #[test]
    fn different_domains_different_hashes() {
        let a = domain_hash(b"DomainA", b"data");
        let b = domain_hash(b"DomainB", b"data");
        assert_ne!(a, b);
    }

    #[test]
    fn state_hash_deterministic() {
        let a = state_hash(b"some resource data");
        let b = state_hash(b"some resource data");
        assert_eq!(a, b);
        assert_ne!(a, state_hash(b"different data"));
    }
}
