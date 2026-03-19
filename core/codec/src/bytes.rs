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

    /// Sets the bit at `index` using MSB-first ordering (in place). Returns `&mut Self` for
    /// chaining.
    fn set_msb(&mut self, index: usize) -> &mut Self;

    /// Sets the bit at `index` using LSB-first ordering (in place). Returns `&mut Self` for
    /// chaining.
    fn set_lsb(&mut self, index: usize) -> &mut Self;
}

impl Bits for [u8] {
    fn get_msb(&self, index: usize) -> bool {
        index / 8 < self.len() && (self[index / 8] >> (7 - index % 8)) & 1 == 1
    }

    fn get_lsb(&self, index: usize) -> bool {
        index / 8 < self.len() && (self[index / 8] >> (index % 8)) & 1 == 1
    }

    fn set_msb(&mut self, index: usize) -> &mut Self {
        if index / 8 < self.len() {
            self[index / 8] |= 1 << (7 - index % 8);
        }
        self
    }

    fn set_lsb(&mut self, index: usize) -> &mut Self {
        if index / 8 < self.len() {
            self[index / 8] |= 1 << (index % 8);
        }
        self
    }
}
