use std::sync::Arc;

use vprogs_core_atomics::AtomicRing;

use crate::bucket::Bucket;

/// An immutable-perception snapshot of the canonical bits.
///
/// Its `tip` and the bits it reaches are frozen at publication; the shared `body` ring's growth and
/// head-pruning stay visible but never change a queryable answer.
pub struct View {
    /// Highest canonical id and the upper bound for reads. `0` means empty.
    pub(crate) tip: u64,
    /// Bucket number of `tail` (the highest allocated bucket).
    pub(crate) tail_bucket: u64,
    /// The bucket currently being filled (number `tail_bucket`).
    pub(crate) tail: Arc<Bucket>,
    /// The previous bucket (number `tail_bucket - 1`), kept hot so a shallow reorg avoids the
    /// body.
    pub(crate) last_sealed: Arc<Bucket>,
    /// Sealed buckets `0 ..= tail_bucket - 2`; a bucket pruned off the head reads as canonical.
    pub(crate) body: Arc<AtomicRing<Arc<Bucket>>>,
}

impl View {
    /// Returns whether `id` is canonical here; ids are 1-based and `0` is never canonical.
    pub fn is_canonical(&self, id: u64) -> bool {
        if id == 0 || id > self.tip {
            return false;
        }
        let (bucket, bit) = locate(id);
        if bucket == self.tail_bucket {
            return self.tail.get(bit);
        }
        if self.tail_bucket >= 1 && bucket == self.tail_bucket - 1 {
            return self.last_sealed.get(bit);
        }
        // An absent sealed bucket was pruned off the head, so it is finalized and reads canonical.
        self.body.with(bucket, |sealed| sealed.get(bit)).unwrap_or(true)
    }

    /// Returns the highest canonical id (the read bound).
    pub fn tip(&self) -> u64 {
        self.tip
    }

    /// The empty view: no canonical ids.
    pub(crate) fn empty() -> Self {
        Self {
            tip: 0,
            tail_bucket: 0,
            tail: Arc::new(Bucket::new()),
            last_sealed: Arc::new(Bucket::new()),
            body: Arc::new(AtomicRing::new(0)),
        }
    }
}

/// Maps a 1-based id to its `(bucket number, within-bucket bit)`.
pub(crate) fn locate(id: u64) -> (u64, usize) {
    let zero_based = id - 1;
    (zero_based / Bucket::CAPACITY, (zero_based % Bucket::CAPACITY) as usize)
}
