use crate::Hasher;

/// Blake3 implementation of the `Hasher` trait.
pub struct Blake3;

/// Incremental BLAKE3 state, wrapping [`blake3::Hasher`].
#[derive(Default)]
pub struct Blake3Incremental(blake3::Hasher);

impl crate::IncrementalHasher for Blake3Incremental {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

impl Hasher for Blake3 {
    type Incremental = Blake3Incremental;

    fn hash(data: impl AsRef<[u8]>) -> [u8; 32] {
        *blake3::hash(data.as_ref()).as_bytes()
    }

    fn hash_parts_with_domain<const N: usize>(
        domain: &[u8; N],
        parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> [u8; 32] {
        const { assert!(N <= 32, "BLAKE3 keyed-mode key is 32 bytes; domain must fit") };
        let mut key = [0u8; 32];
        key[..N].copy_from_slice(domain);
        let mut hasher = blake3::Hasher::new_keyed(&key);
        for part in parts {
            hasher.update(part.as_ref());
        }
        *hasher.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use crate::{Hasher, IncrementalHasher};

    #[test]
    fn incremental_matches_one_shot() {
        let mut incremental = super::Blake3::incremental();
        incremental.update(b"abc");
        incremental.update(b"def");
        assert_eq!(incremental.finalize(), super::Blake3::hash(b"abcdef"));
    }
}
