use std::sync::Arc;

use vprogs_core_atomics::AtomicRing;

use crate::bucket::Bucket;

/// The tail and last-sealed buckets, kept hot so a shallow reorg avoids copying the body ring.
///
/// See [`snapshot`](crate::snapshot) for how the hot zone sits above the body.
pub(crate) struct HotZone {
    /// Bucket number of `tail` (the highest allocated bucket).
    pub(crate) tail_bucket: u64,
    /// The bucket currently being filled (number `tail_bucket`).
    pub(crate) tail: Arc<Bucket>,
    /// The previous bucket (`tail_bucket - 1`), kept hot so a shallow reorg avoids the body.
    pub(crate) last_sealed: Arc<Bucket>,
}

impl HotZone {
    /// The empty hot zone at bucket `0`.
    pub(crate) fn empty() -> Self {
        Self { tail_bucket: 0, tail: Arc::new(Bucket::new()), last_sealed: Arc::new(Bucket::new()) }
    }

    /// Advances to `target`, sealing buckets left behind into `body`, and returns the new hot zone.
    pub(crate) fn roll_forward(&self, target: u64, body: &AtomicRing<Arc<Bucket>>) -> HotZone {
        debug_assert!(target >= self.tail_bucket, "roll_forward must not move the tail backwards");

        // Unchanged target: reuse the current hot buckets.
        if target == self.tail_bucket {
            return HotZone {
                tail_bucket: self.tail_bucket,
                tail: Arc::clone(&self.tail),
                last_sealed: Arc::clone(&self.last_sealed),
            };
        }

        // Seal buckets behind us until the body reaches the new last-sealed slot (`target - 1`).
        while body.next_index() + 1 < target {
            let idx = body.next_index();
            body.push(self.bucket_at(idx));
        }

        // Advance: a fresh empty tail, with bucket `target - 1` as the new last-sealed.
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
