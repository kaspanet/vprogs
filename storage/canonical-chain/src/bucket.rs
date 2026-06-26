use std::sync::Arc;

/// A fixed-size run of canonical bits (`1` = canonical), covering [`Bucket::CAPACITY`] ids.
#[derive(Clone)]
pub(crate) struct Bucket([u64; Bucket::CAPACITY / 64]);

impl Bucket {
    /// Ids a bucket holds, one bit each.
    pub(crate) const CAPACITY: usize = 4096;

    /// Creates a bucket with no canonical bits set (every bit `0`).
    pub(crate) fn new() -> Self {
        Self([0; Self::CAPACITY / 64])
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

    /// Returns a shareable copy with each `(bit, value)` in `ops` applied.
    pub(crate) fn edited(&self, ops: &[(usize, bool)]) -> Arc<Bucket> {
        let mut bucket = self.clone();
        for &(bit, value) in ops {
            bucket.set(bit, value);
        }
        Arc::new(bucket)
    }

    /// Maps a 1-based id to its `(bucket number, within-bucket bit)`.
    pub(crate) fn locate(id: u64) -> (u64, usize) {
        let zero_based = id - 1;
        let cap = Self::CAPACITY as u64;
        (zero_based / cap, (zero_based % cap) as usize)
    }
}
