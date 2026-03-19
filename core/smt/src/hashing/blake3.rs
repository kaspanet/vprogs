use super::hasher::Hasher;

/// Blake3 implementation of the `Hasher` trait.
pub struct Blake3;

impl Hasher for Blake3 {
    fn hash(data: &[u8]) -> [u8; 32] {
        *blake3::hash(data).as_bytes()
    }
}
