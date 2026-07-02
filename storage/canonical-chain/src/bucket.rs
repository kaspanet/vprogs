use std::{
    array::from_fn,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// Ids a bucket holds, one bit each.
pub const CAPACITY: u64 = 128;

/// A fixed-size run of canonical bits (`1` = canonical), covering [`CAPACITY`] ids.
pub(crate) struct Bucket([AtomicU64; (CAPACITY / 64) as usize]);

impl Bucket {
    /// Creates a bucket with no canonical bits set (every bit `0`).
    pub(crate) fn new() -> Self {
        Self(from_fn(|_| AtomicU64::new(0)))
    }

    /// Returns whether the within-bucket bit `bit` is set.
    pub(crate) fn get(&self, bit: usize) -> bool {
        debug_assert!(bit < CAPACITY as usize);
        (self.0[bit / 64].load(Ordering::Relaxed) >> (bit % 64)) & 1 == 1
    }

    /// Marks the within-bucket bit `bit` canonical, atomically and in place.
    pub(crate) fn set(&self, bit: usize) {
        debug_assert!(bit < CAPACITY as usize);
        self.0[bit / 64].fetch_or(1u64 << (bit % 64), Ordering::Relaxed);
    }

    /// Returns an independent copy with each `(bit, value)` in `ops` applied (copy-on-write).
    pub(crate) fn edited(&self, ops: &[(usize, bool)]) -> Arc<Bucket> {
        // Edit plain words; the copy is unshared until sealed, so no atomic RMW is needed.
        let mut words = self.snapshot();
        for &(bit, value) in ops {
            debug_assert!(bit < CAPACITY as usize);
            let mask = 1u64 << (bit % 64);
            if value {
                words[bit / 64] |= mask;
            } else {
                words[bit / 64] &= !mask;
            }
        }

        // Seal into an atomic bucket, now safe to share.
        Arc::new(Self(from_fn(|w| AtomicU64::new(words[w]))))
    }

    /// Maps a 1-based id to its `(bucket number, within-bucket bit)`.
    pub(crate) fn locate(id: u64) -> (u64, usize) {
        let zero_based = id - 1;
        (zero_based / CAPACITY, (zero_based % CAPACITY) as usize)
    }

    /// A plain (non-atomic) snapshot of the bucket's words.
    fn snapshot(&self) -> [u64; (CAPACITY / 64) as usize] {
        from_fn(|w| self.0[w].load(Ordering::Relaxed))
    }
}
