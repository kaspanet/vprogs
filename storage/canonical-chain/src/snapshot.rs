use std::sync::Arc;

use vprogs_core_atomics::AtomicRing;

use crate::{bucket::Bucket, hot_zone::HotZone};

/// A snapshot of the canonical bits, frozen at publication so its answers never change.
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
        // Out of range: 0 is never canonical, and nothing above the tip exists yet.
        if id == 0 || id > self.tip {
            return false;
        }

        // Locate the bucket and the bit within it.
        let (bucket, bit) = Bucket::locate(id);

        // Hot zone: the tail and last-sealed buckets carry the live bits.
        let hot = &self.hot_zone;
        if bucket == hot.tail_bucket {
            return hot.tail.get(bit);
        }
        if hot.tail_bucket >= 1 && bucket == hot.tail_bucket - 1 {
            return hot.last_sealed.get(bit);
        }

        // Body: read the sealed bit; an absent (pruned-off, finalized) bucket reads canonical.
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
