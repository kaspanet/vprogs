use super::hasher::Hasher;

/// Blake3 implementation of the `Hasher` trait.
pub struct Blake3Hasher;

impl Hasher for Blake3Hasher {
    fn hash(data: &[u8]) -> [u8; 32] {
        *blake3::hash(data).as_bytes()
    }
}
