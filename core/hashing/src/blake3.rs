use crate::Hasher;

/// Blake3 implementation of the `Hasher` trait.
pub struct Blake3;

impl Hasher for Blake3 {
    fn hash(data: impl AsRef<[u8]>) -> [u8; 32] {
        *blake3::hash(data.as_ref()).as_bytes()
    }

    fn hash_parts_with_domain<const N: usize>(
        domain: &[u8; N],
        parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(domain);
        for part in parts {
            hasher.update(part.as_ref());
        }
        *hasher.finalize().as_bytes()
    }
}
