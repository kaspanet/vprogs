/// Extension trait for bitwise access on byte slices.
///
/// Provides MSB-first and LSB-first bit indexing. MSB-first matches the tree's left/right
/// convention (bit 0 is the most significant bit of byte 0). LSB-first matches the topology
/// bitfield wire format.
pub trait Bits {
    /// Returns the bit at `index` using MSB-first ordering (bit 0 = MSB of byte 0).
    ///
    /// Returns `false` for out-of-bounds indices.
    fn get_msb(&self, index: usize) -> bool;

    /// Returns the bit at `index` using LSB-first ordering (bit 0 = LSB of byte 0).
    ///
    /// Returns `false` for out-of-bounds indices.
    fn get_lsb(&self, index: usize) -> bool;

    /// Sets the bit at `index` using LSB-first ordering (in place).
    fn set_lsb(&mut self, index: usize);
}

impl Bits for [u8] {
    fn get_msb(&self, index: usize) -> bool {
        let byte_idx = index / 8;
        let bit_offset = 7 - (index % 8); // MSB-first within each byte.
        byte_idx < self.len() && (self[byte_idx] >> bit_offset) & 1 == 1
    }

    fn get_lsb(&self, index: usize) -> bool {
        let byte_idx = index / 8;
        let bit_offset = index % 8; // LSB-first within each byte.
        byte_idx < self.len() && (self[byte_idx] >> bit_offset) & 1 == 1
    }

    fn set_lsb(&mut self, index: usize) {
        self[index / 8] |= 1 << (index % 8);
    }
}

/// Copy-on-write bit operations for 32-byte arrays.
///
/// Returns a new array with the modification applied, matching the common pattern of copying a
/// value before setting bits.
pub trait BitsArray {
    /// Returns a copy with bit `index` set (MSB-first ordering).
    fn with_bit_set(self, index: usize) -> Self;
}

impl BitsArray for [u8; 32] {
    fn with_bit_set(mut self, index: usize) -> Self {
        let byte_idx = index / 8;
        let bit_offset = 7 - (index % 8);
        self[byte_idx] |= 1 << bit_offset;
        self
    }
}
