use alloc::vec;

use super::bytes::Bits;

/// Extension trait for packing boolean slices into byte vectors.
pub(crate) trait Bools {
    /// Packs bools into a byte vector using LSB-first ordering within each byte.
    fn pack_lsb(&self) -> alloc::vec::Vec<u8>;
}

impl Bools for [bool] {
    fn pack_lsb(&self) -> alloc::vec::Vec<u8> {
        let mut bytes = vec![0u8; self.len().div_ceil(8)];
        for (i, &bit) in self.iter().enumerate() {
            if bit {
                bytes.set_lsb(i);
            }
        }
        bytes
    }
}
