use std::mem::size_of;

/// A fixed-size run of canonical bits (`1` = canonical), covering [`Bucket::CAPACITY`] ids.
#[derive(Clone)]
pub(crate) struct Bucket([u64; Bucket::SIZE / Bucket::WORD_SIZE]);

impl Bucket {
    /// Ids a bucket holds, one bit each.
    pub(crate) const CAPACITY: u64 = 4096;

    /// Bytes per backing `u64` word.
    const WORD_SIZE: usize = size_of::<u64>();

    /// Bucket size in bytes; equals `size_of::<Bucket>()` and the persisted-blob size.
    pub(crate) const SIZE: usize = Self::CAPACITY as usize / 8;

    /// Creates a bucket with no canonical bits set (every bit `0`).
    pub(crate) fn new() -> Self {
        Self([0; Self::SIZE / Self::WORD_SIZE])
    }

    /// Returns whether the within-bucket bit `bit` is set.
    pub(crate) fn get(&self, bit: usize) -> bool {
        (self.0[bit / 64] >> (bit % 64)) & 1 == 1
    }

    /// Sets the within-bucket bit `bit` to `value`.
    pub(crate) fn set(&mut self, bit: usize, value: bool) {
        let word = &mut self.0[bit / 64];
        let mask = 1u64 << (bit % 64);
        if value {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }
}
