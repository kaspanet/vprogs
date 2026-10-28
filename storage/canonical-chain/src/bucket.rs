use std::{
    array::from_fn,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// Ids a bucket holds, one bit each.
pub const CAPACITY: u64 = 128;

/// 64-bit words per bucket.
pub(crate) const WORDS: usize = (CAPACITY / 64) as usize;

/// The raw canonical bits of one bucket.
pub type BucketWords = [u64; WORDS];

/// One bucket's frozen canonical bits, ready to persist at finalization and replay into restore.
pub struct FrozenBits {
    /// The bucket number the words belong to.
    pub bucket: u64,
    /// The bucket's canonical bits.
    pub words: BucketWords,
}

/// A fixed-size run of canonical bits (`1` = canonical), covering [`CAPACITY`] ids.
pub(crate) struct Bucket([AtomicU64; WORDS]);

impl Bucket {
    /// Creates a bucket with no canonical bits set (every bit `0`).
    pub(crate) fn new() -> Self {
        Self(from_fn(|_| AtomicU64::new(0)))
    }

    /// Creates a bucket with every bit set (all ids canonical).
    pub(crate) fn all_canonical() -> Self {
        Self(from_fn(|_| AtomicU64::new(u64::MAX)))
    }

    /// Creates a bucket carrying exactly `words`.
    pub(crate) fn from_words(words: BucketWords) -> Self {
        Self(from_fn(|w| AtomicU64::new(words[w])))
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
        let mut words = self.words();
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

    /// A plain (non-atomic) read of the bucket's words.
    pub(crate) fn words(&self) -> BucketWords {
        from_fn(|w| self.0[w].load(Ordering::Relaxed))
    }
}

/// `words` with only the bits `[0, bit)` kept.
pub(crate) fn sub_base_words(words: BucketWords, bit: usize) -> BucketWords {
    let mut masked = [0u64; WORDS];
    for w in 0..WORDS {
        let keep = if w < bit / 64 {
            u64::MAX
        } else if w == bit / 64 {
            (1u64 << (bit % 64)) - 1
        } else {
            0
        };
        masked[w] = words[w] & keep;
    }
    masked
}
