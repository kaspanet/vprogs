use std::sync::Arc;

use vprogs_core_atomics::AtomicRing;

use crate::bucket::Bucket;

/// An immutable-perception snapshot of the canonical bits.
///
/// Its `tip` and the bits it reaches are frozen at publication; the shared `body` ring's growth and
/// head-pruning stay visible but never change a queryable answer.
pub struct CanonicalChainSnapshot {
    /// Highest canonical id and the upper bound for reads. `0` means empty.
    pub(crate) tip: u64,
    /// The recent, copy-on-write buckets (the tail and the last-sealed bucket).
    pub(crate) hot_zone: HotZone,
    /// Sealed buckets `0 ..= tail_bucket - 2`; a bucket pruned off the head reads as canonical.
    pub(crate) body: Arc<AtomicRing<Arc<Bucket>>>,
}

impl CanonicalChainSnapshot {
    /// Returns whether `id` is canonical here; ids are 1-based and `0` is never canonical.
    pub fn is_canonical(&self, id: u64) -> bool {
        if id == 0 || id > self.tip {
            return false;
        }
        let (bucket, bit) = Bucket::locate(id);
        let hot = &self.hot_zone;
        if bucket == hot.tail_bucket {
            return hot.tail.get(bit);
        }
        if hot.tail_bucket >= 1 && bucket == hot.tail_bucket - 1 {
            return hot.last_sealed.get(bit);
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
        Self { tip: 0, hot_zone: HotZone::empty(), body: Arc::new(AtomicRing::new(0)) }
    }
}

/// The recent, copy-on-write part of a snapshot: the tail bucket and the last-sealed bucket, kept
/// hot so a shallow reorg avoids the body ring.
pub(crate) struct HotZone {
    /// Bucket number of `tail` (the highest allocated bucket).
    pub(crate) tail_bucket: u64,
    /// The bucket currently being filled (number `tail_bucket`).
    pub(crate) tail: Arc<Bucket>,
    /// The previous bucket (number `tail_bucket - 1`), kept hot so a shallow reorg avoids the
    /// body.
    pub(crate) last_sealed: Arc<Bucket>,
}

impl HotZone {
    /// The empty hot zone at bucket `0`.
    pub(crate) fn empty() -> Self {
        Self { tail_bucket: 0, tail: Arc::new(Bucket::new()), last_sealed: Arc::new(Bucket::new()) }
    }

    /// Advances to `target`, sealing buckets left behind into `body`, and returns the new hot zone.
    /// `target` must not move the tail backwards.
    pub(crate) fn roll_forward(&self, target: u64, body: &AtomicRing<Arc<Bucket>>) -> HotZone {
        debug_assert!(target >= self.tail_bucket, "roll_forward must not move the tail backwards");
        if target == self.tail_bucket {
            return HotZone {
                tail_bucket: self.tail_bucket,
                tail: Arc::clone(&self.tail),
                last_sealed: Arc::clone(&self.last_sealed),
            };
        }

        // Seal buckets until the body's next index reaches the new last-sealed slot (target - 1).
        while body.next_index() + 1 < target {
            let idx = body.next_index();
            body.push(self.bucket_at(idx));
        }

        HotZone {
            tail_bucket: target,
            tail: Arc::new(Bucket::new()),
            last_sealed: self.bucket_at(target - 1),
        }
    }

    /// The bucket at index `idx`, or a fresh empty bucket beyond the hot zone.
    fn bucket_at(&self, idx: u64) -> Arc<Bucket> {
        if idx == self.tail_bucket {
            Arc::clone(&self.tail)
        } else if self.tail_bucket >= 1 && idx == self.tail_bucket - 1 {
            Arc::clone(&self.last_sealed)
        } else {
            Arc::new(Bucket::new())
        }
    }
}
