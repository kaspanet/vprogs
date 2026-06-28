use std::{
    array::from_fn,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// Ids a bucket holds, one bit each.
pub const CAPACITY: u64 = 4096;

/// A fixed-size run of canonical bits (`1` = canonical), covering [`CAPACITY`] ids.
pub(crate) struct Bucket([AtomicU64; (CAPACITY / 64) as usize]);

impl Bucket {
    /// Creates a bucket with no canonical bits set (every bit `0`).
    pub(crate) fn new() -> Self {
        Self(from_fn(|_| AtomicU64::new(0)))
    }

    /// Returns whether the within-bucket bit `bit` is set.
    pub(crate) fn get(&self, bit: usize) -> bool {
        (self.0[bit / 64].load(Ordering::Relaxed) >> (bit % 64)) & 1 == 1
    }

    /// Sets the within-bucket bit `bit` to `value`, atomically and in place.
    pub(crate) fn set(&self, bit: usize, value: bool) {
        let word = &self.0[bit / 64];
        let mask = 1u64 << (bit % 64);
        if value {
            word.fetch_or(mask, Ordering::Relaxed);
        } else {
            word.fetch_and(!mask, Ordering::Relaxed);
        }
    }

    /// Returns an independent copy with each `(bit, value)` in `ops` applied (copy-on-write).
    pub(crate) fn edited(&self, ops: &[(usize, bool)]) -> Arc<Bucket> {
        let bucket = Self(from_fn(|w| AtomicU64::new(self.0[w].load(Ordering::Relaxed))));
        for &(bit, value) in ops {
            bucket.set(bit, value);
        }
        Arc::new(bucket)
    }

    /// Maps a 1-based id to its `(bucket number, within-bucket bit)`.
    pub(crate) fn locate(id: u64) -> (u64, usize) {
        let zero_based = id - 1;
        (zero_based / CAPACITY, (zero_based % CAPACITY) as usize)
    }
}
