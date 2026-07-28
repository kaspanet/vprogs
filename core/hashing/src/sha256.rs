use sha2::Digest;

use crate::Hasher;

/// SHA-256 implementation of the [`Hasher`] trait.
pub struct Sha256;

/// Incremental SHA-256 state, wrapping [`sha2::Sha256`].
#[derive(Default)]
pub struct Sha256Incremental(sha2::Sha256);

impl crate::IncrementalHasher for Sha256Incremental {
    fn update(&mut self, data: &[u8]) {
        Digest::update(&mut self.0, data);
    }

    fn finalize(self) -> [u8; 32] {
        Digest::finalize(self.0).into()
    }
}

impl Hasher for Sha256 {
    type Incremental = Sha256Incremental;

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

#[cfg(test)]
mod tests {
    use crate::{Hasher, IncrementalHasher};

    #[test]
    fn incremental_matches_one_shot() {
        let mut incremental = super::Sha256::incremental();
        incremental.update(b"abc");
        incremental.update(b"def");
        assert_eq!(incremental.finalize(), super::Sha256::hash(b"abcdef"));
    }
}
