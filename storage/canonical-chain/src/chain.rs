use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use arc_swap::ArcSwap;
use vprogs_core_atomics::AtomicRing;

use crate::{
    bucket::Bucket,
    snapshot::{CanonicalChainSnapshot, HotZone},
};

/// The lock-free canonical-chain oracle; its sole writer is the `CanonicalChainManager`.
#[derive(Clone)]
pub struct CanonicalChain {
    /// The current snapshot, atomically published on each mutation and shared across clones.
    current: Arc<ArcSwap<CanonicalChainSnapshot>>,
    /// Set while a `CanonicalChainManager` drives this chain; enforces a single writer.
    writer: Arc<AtomicBool>,
}

impl CanonicalChain {
    /// Returns the current highest canonical id. Wait-free.
    pub fn tip(&self) -> u64 {
        self.current.load().tip
    }

    /// Returns a consistent view a reader can run a whole operation against.
    pub fn snapshot(&self) -> Arc<CanonicalChainSnapshot> {
        self.current.load_full()
    }

    /// Claims the sole-writer role; panics if another writer already holds it.
    pub(crate) fn claim_writer(&self) {
        assert!(
            !self.writer.swap(true, Ordering::AcqRel),
            "a CanonicalChainManager already drives this chain"
        );
    }

    /// Releases the sole-writer role so a new writer can claim it.
    pub(crate) fn release_writer(&self) {
        self.writer.store(false, Ordering::Release);
    }

    /// Restores the `canonical` ids over `base..=tip` at startup.
    pub(crate) fn restore(&self, base: u64, tip: u64, canonical: impl IntoIterator<Item = u64>) {
        if tip == 0 {
            return;
        }
        let base_bucket = Bucket::locate(base).0;
        let tail_bucket = Bucket::locate(tip).0;

        // One owned bucket per live bucket number, with the canonical bits set in place.
        let mut buckets = vec![Bucket::new(); (tail_bucket - base_bucket + 1) as usize];
        for id in canonical {
            let (bucket, bit) = Bucket::locate(id);
            buckets[(bucket - base_bucket) as usize].set(bit, true);
        }

        // Peel the hot zone (tail, then last_sealed) off the top; seal the rest into a ring based
        // at the live floor so lower buckets read as absent (finalized).
        let tail = Arc::new(buckets.pop().expect("live range has at least one bucket"));
        let last_sealed = buckets.pop().map_or_else(|| Arc::new(Bucket::new()), Arc::new);
        let body = AtomicRing::new(base_bucket);
        for bucket in buckets {
            body.push(Arc::new(bucket));
        }

        self.current.store(Arc::new(CanonicalChainSnapshot {
            tip,
            hot_zone: HotZone { tail_bucket, tail, last_sealed },
            body: Arc::new(body),
        }));
    }

    /// Marks `id` canonical as the new tip. Debug-panics unless `id` > tip.
    pub(crate) fn append(&self, id: u64) {
        let cur = self.current.load();
        debug_assert!(id > cur.tip, "append id {id} must extend tip {}", cur.tip);

        // Appends only grow the shared body (a monotonic seal), so reuse the same ring.
        let (bucket, bit) = Bucket::locate(id);
        let body = Arc::clone(&cur.body);
        let mut hot_zone = cur.hot_zone.roll_forward(bucket, &body);
        hot_zone.tail = hot_zone.tail.edited(&[(bit, true)]);

        self.current.store(Arc::new(CanonicalChainSnapshot { tip: id, hot_zone, body }));
    }

    /// Rolls the chain back to `new_tip`, flipping off every id above it.
    pub(crate) fn rollback(&self, new_tip: u64) {
        let cur = self.current.load();
        debug_assert!(new_tip <= cur.tip, "rollback target {new_tip} exceeds tip {}", cur.tip);

        // Rewriting a sealed bucket mutates shared state, so fork the body; hot-zone buckets stay
        // shareable. The orphaned ids are the dense suffix above new_tip, so the lowest one
        // (new_tip + 1) alone decides whether any sealed bucket is touched.
        let deep = Bucket::locate(new_tip + 1).0 + 2 <= cur.hot_zone.tail_bucket;
        let body = if deep { Arc::new(cur.body.fork()) } else { Arc::clone(&cur.body) };

        let mut hot_zone = cur.hot_zone.roll_forward(cur.hot_zone.tail_bucket, &body);

        // Group the bit edits by bucket so each touched bucket is rewritten once.
        let mut edits: BTreeMap<u64, Vec<(usize, bool)>> = BTreeMap::new();
        for id in new_tip + 1..=cur.tip {
            let (bucket, bit) = Bucket::locate(id);
            edits.entry(bucket).or_default().push((bit, false));
        }

        // Hot-zone buckets copy-on-write; body buckets are replaced in the (forked) ring; an edit
        // for a bucket already pruned off the head is below the horizon and dropped.
        for (bucket, ops) in edits {
            if bucket == hot_zone.tail_bucket {
                hot_zone.tail = hot_zone.tail.edited(&ops);
            } else if hot_zone.tail_bucket >= 1 && bucket == hot_zone.tail_bucket - 1 {
                hot_zone.last_sealed = hot_zone.last_sealed.edited(&ops);
            } else if let Some(sealed) = body.get(bucket) {
                body.replace(bucket, sealed.edited(&ops));
            }
        }

        self.current.store(Arc::new(CanonicalChainSnapshot { tip: new_tip, hot_zone, body }));
    }

    /// Finalizes ids below `below`, which thereafter read as canonical.
    pub(crate) fn finalize(&self, below: u64) {
        if below > 1 {
            // Keep the bucket containing `below`; drop every strictly lower (finalized) bucket.
            self.current.load().body.prune_below(Bucket::locate(below).0);
        }
    }
}

impl Default for CanonicalChain {
    /// Creates an empty chain with no canonical ids yet.
    fn default() -> Self {
        Self {
            current: Arc::new(ArcSwap::from_pointee(CanonicalChainSnapshot::empty())),
            writer: Arc::new(AtomicBool::new(false)),
        }
    }
}
