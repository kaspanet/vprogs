use sha2::Digest;

use crate::Hasher;

/// SHA-256 implementation of the [`Hasher`] trait.
pub struct Sha256;

impl Hasher for Sha256 {
    fn hash(data: impl AsRef<[u8]>) -> [u8; 32] {
        sha2::Sha256::digest(data.as_ref()).into()
    }

    /// Prepends the domain bytes to the payload (SHA-256 has no native keyed mode). Equivalent to
    /// `sha2::Sha256::new_with_prefix(domain).update(part).finalize()` byte-for-byte.
    fn hash_parts_with_domain<const N: usize>(
        domain: &[u8; N],
        parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> [u8; 32] {
        let mut hasher = sha2::Sha256::new();
        hasher.update(domain);
        for part in parts {
            hasher.update(part.as_ref());
        }
        hasher.finalize().into()
    }
}
