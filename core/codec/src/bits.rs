/// Bitwise access on byte slices using two indexing conventions.
///
/// MSB-first: bit 0 is the most significant bit of byte 0. LSB-first: bit 0 is the least
/// significant bit of byte 0. Both conventions index the same bytes - the difference is which bit
/// within each byte a given index addresses.
///
/// Out-of-bounds access is a no-op for setters and returns `false` for getters.
pub trait Bits {
    /// Returns the bit at `index` (MSB-first).
    fn get_msb(&self, index: usize) -> bool;

    /// Returns the bit at `index` (LSB-first).
    fn get_lsb(&self, index: usize) -> bool;

    /// Sets the bit at `index` (MSB-first). Returns `&mut Self` for chaining.
    fn set_msb(&mut self, index: usize) -> &mut Self;

    /// Sets the bit at `index` (LSB-first). Returns `&mut Self` for chaining.
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
