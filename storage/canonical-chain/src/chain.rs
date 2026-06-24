use std::{collections::BTreeMap, sync::Arc};

use arc_swap::ArcSwap;
use vprogs_core_atomics::AtomicRing;

use crate::{
    bucket::Bucket,
    view::{View, locate},
};

/// The lock-free canonical-chain oracle over a monotonic id space.
///
/// Canonical-ness is one bit per id, held in a rolling ring of fixed-size buckets so a query
/// decides without touching disk. A reader takes a [`snapshot`](Self::snapshot) whose perception is
/// frozen against later reorgs while data growth/pruning stays shared. The public API is read-only;
/// the single-writer [`CanonicalWriter`](crate::CanonicalWriter) drives the `pub(crate)` mutators.
#[derive(Clone)]
pub struct CanonicalChain {
    view: Arc<ArcSwap<View>>,
}

impl CanonicalChain {
    /// Creates an empty chain with no canonical ids yet.
    pub fn new() -> Self {
        Self { view: Arc::new(ArcSwap::from_pointee(View::empty())) }
    }

    /// Bulk-builds the canonical bits for the `canonical` ids in a single publish, replacing the
    /// current view. Single-writer, for restoring at startup.
    ///
    /// Unlike a sequence of [`append`](Self::append)s this sets every bit in owned buckets and
    /// publishes once, so it never copies a bucket per id. `tip` is the highest canonical id;
    /// buckets fully below `base` are left absent so they read as finalized.
    pub(crate) fn restore(&self, base: u64, tip: u64, canonical: impl IntoIterator<Item = u64>) {
        if tip == 0 {
            return;
        }
        let base_bucket = locate(base).0;
        let tail_bucket = locate(tip).0;

        // One owned bucket per live bucket number, with the canonical bits set in place.
        let mut buckets = vec![Bucket::new(); (tail_bucket - base_bucket + 1) as usize];
        for id in canonical {
            let (bucket, bit) = locate(id);
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

        self.publish(View { tip, tail_bucket, last_sealed, tail, body: Arc::new(body) });
    }

    /// Returns whether `id` is currently canonical. Wait-free.
    pub fn is_canonical(&self, id: u64) -> bool {
        self.view.load().is_canonical(id)
    }

    /// Returns the current highest canonical id. Wait-free.
    pub fn tip(&self) -> u64 {
        self.view.load().tip
    }

    /// Returns a consistent view a reader can run a whole operation against.
    pub fn snapshot(&self) -> Arc<View> {
        self.view.load_full()
    }

    /// Marks `id` canonical and extends the tip to it. Single-writer; debug-panics if `id` does not
    /// extend the tip.
    pub(crate) fn append(&self, id: u64) {
        let cur = self.view.load();
        debug_assert!(id > cur.tip, "append id {id} must extend tip {}", cur.tip);

        // Appends only grow the shared body (a monotonic seal), so reuse the same ring.
        let (bucket, bit) = locate(id);
        let body = Arc::clone(&cur.body);
        let (tail_bucket, last_sealed, tail) = roll_forward(&cur, bucket, &body);

        self.publish(View {
            tip: id,
            tail_bucket,
            last_sealed,
            tail: edited(&tail, &[(bit, true)]),
            body,
        });
    }

    /// Rolls the chain back to `new_tip`, flipping off every id above it. Single-writer.
    ///
    /// A rollback that rewrites a sealed body bucket forks the body so existing views keep their
    /// perception; one confined to the hot zone shares it.
    pub(crate) fn rollback(&self, new_tip: u64) {
        let cur = self.view.load();
        debug_assert!(new_tip <= cur.tip, "rollback target {new_tip} exceeds tip {}", cur.tip);

        // Rewriting a sealed bucket mutates shared state, so fork the body; hot-zone buckets stay
        // shareable. The orphaned ids are the dense suffix above new_tip, so the lowest one
        // (new_tip + 1) alone decides whether any sealed bucket is touched.
        let deep = locate(new_tip + 1).0 + 2 <= cur.tail_bucket;
        let body: Arc<AtomicRing<Arc<Bucket>>> =
            if deep { Arc::new(cur.body.fork()) } else { Arc::clone(&cur.body) };

        let (tail_bucket, mut last_sealed, mut tail) = roll_forward(&cur, cur.tail_bucket, &body);

        // Group the bit edits by bucket so each touched bucket is rewritten once.
        let mut edits: BTreeMap<u64, Vec<(usize, bool)>> = BTreeMap::new();
        for id in new_tip + 1..=cur.tip {
            let (bucket, bit) = locate(id);
            edits.entry(bucket).or_default().push((bit, false));
        }

        // Hot-zone buckets copy-on-write; body buckets are replaced in the (forked) ring; an edit
        // for a bucket already pruned off the head is below the horizon and dropped.
        for (bucket, ops) in edits {
            if bucket == tail_bucket {
                tail = edited(&tail, &ops);
            } else if tail_bucket >= 1 && bucket == tail_bucket - 1 {
                last_sealed = edited(&last_sealed, &ops);
            } else if let Some(sealed) = body.get(bucket) {
                body.replace(bucket, edited(&sealed, &ops));
            }
        }

        self.publish(View { tip: new_tip, tail_bucket, last_sealed, tail, body });
    }

    /// Finalizes ids below `below`: buckets entirely below it are pruned off the body head and
    /// thereafter read as canonical. Single-writer.
    pub(crate) fn finalize(&self, below: u64) {
        if below <= 1 {
            return;
        }
        // Keep the bucket containing `below`; drop every strictly lower (fully finalized) bucket.
        let keep_from = locate(below).0;
        self.view.load().body.prune_below(keep_from);
    }

    /// Publishes a new view, replacing the live one atomically.
    fn publish(&self, view: View) {
        self.view.store(Arc::new(view));
    }
}

impl Default for CanonicalChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns a copy of `base` with `ops` applied.
fn edited(base: &Bucket, ops: &[(usize, bool)]) -> Arc<Bucket> {
    let mut bucket = base.clone();
    for &(bit, value) in ops {
        bucket.set(bit, value);
    }
    Arc::new(bucket)
}

/// The bucket living at index `idx` in `cur`'s hot zone, or a fresh empty bucket beyond it.
fn bucket_at(cur: &View, idx: u64) -> Arc<Bucket> {
    if idx == cur.tail_bucket {
        Arc::clone(&cur.tail)
    } else if cur.tail_bucket >= 1 && idx == cur.tail_bucket - 1 {
        Arc::clone(&cur.last_sealed)
    } else {
        Arc::new(Bucket::new())
    }
}

/// Advances the hot zone to `target`, sealing buckets left behind into `body`, and returns the new
/// `(tail_bucket, last_sealed, tail)`. `target` must not move the tail backwards.
fn roll_forward(
    cur: &View,
    target: u64,
    body: &AtomicRing<Arc<Bucket>>,
) -> (u64, Arc<Bucket>, Arc<Bucket>) {
    debug_assert!(target >= cur.tail_bucket, "roll_forward must not move the tail backwards");
    if target == cur.tail_bucket {
        return (cur.tail_bucket, Arc::clone(&cur.last_sealed), Arc::clone(&cur.tail));
    }

    // Seal buckets until the body's next index reaches the new last-sealed slot (target - 1).
    while body.next_index() + 1 < target {
        let idx = body.next_index();
        body.push(bucket_at(cur, idx));
    }

    (target, bucket_at(cur, target - 1), Arc::new(Bucket::new()))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use super::CanonicalChain;

    #[test]
    fn empty_chain_has_nothing_canonical() {
        let chain = CanonicalChain::new();
        assert_eq!(chain.tip(), 0);
        assert!(!chain.is_canonical(0));
        assert!(!chain.is_canonical(1));
    }

    #[test]
    fn restore_builds_a_sparse_canonical_set_in_one_publish() {
        let chain = CanonicalChain::new();
        // Canonical ids 1, 4, 6, 7 within the live range [1, 7]; 2, 3, 5 are orphan.
        chain.restore(1, 7, [1, 4, 6, 7]);

        assert_eq!(chain.tip(), 7);
        assert!(chain.is_canonical(1));
        assert!(!chain.is_canonical(2));
        assert!(!chain.is_canonical(3));
        assert!(chain.is_canonical(4));
        assert!(!chain.is_canonical(5));
        assert!(chain.is_canonical(6));
        assert!(chain.is_canonical(7));
        assert!(!chain.is_canonical(8));
    }

    #[test]
    fn restore_spans_buckets_and_reads_below_base_as_finalized() {
        let chain = CanonicalChain::new();
        // base in bucket 1, tip in bucket 2; canonical sparse across the two live buckets.
        chain.restore(5_000, 9_000, [5_000, 8_192, 9_000]);

        assert_eq!(chain.tip(), 9_000);
        assert!(chain.is_canonical(5_000));
        assert!(chain.is_canonical(8_192));
        assert!(chain.is_canonical(9_000));
        assert!(!chain.is_canonical(8_193));
        assert!(!chain.is_canonical(9_001));
        // A bucket fully below the live floor reads as finalized-canonical.
        assert!(chain.is_canonical(100));
    }

    #[test]
    fn linear_appends_are_all_canonical() {
        let chain = CanonicalChain::new();
        chain.append(1);
        chain.append(2);
        chain.append(3);

        assert_eq!(chain.tip(), 3);
        assert!(chain.is_canonical(1));
        assert!(chain.is_canonical(3));
        assert!(!chain.is_canonical(4));
        assert!(!chain.is_canonical(0));
    }

    #[test]
    fn appending_past_a_gap_leaves_skipped_ids_orphan() {
        let chain = CanonicalChain::new();
        chain.append(1);
        chain.append(4);

        assert_eq!(chain.tip(), 4);
        assert!(chain.is_canonical(1));
        assert!(chain.is_canonical(4));
        assert!(!chain.is_canonical(2));
        assert!(!chain.is_canonical(3));
    }

    #[test]
    fn rollback_clears_the_orphaned_suffix() {
        let chain = CanonicalChain::new();
        chain.append(1);
        chain.append(2);
        chain.append(3);

        chain.rollback(1);
        assert_eq!(chain.tip(), 1);
        assert!(!chain.is_canonical(2));

        chain.append(4);
        assert!(!chain.is_canonical(2));
        assert!(!chain.is_canonical(3));
        assert!(chain.is_canonical(4));
    }

    #[test]
    fn rollback_then_reappend_revives_orphaned_ids() {
        let chain = CanonicalChain::new();
        chain.append(1);
        chain.append(2);
        chain.append(3);

        // Fork away from 1: orphan 2, 3 and build a different branch.
        chain.rollback(1);
        chain.append(4);

        // Reorg back: roll to the fork and re-append the original branch, reviving 2 and 3.
        chain.rollback(1);
        chain.append(2);
        chain.append(3);
        chain.append(5);

        assert_eq!(chain.tip(), 5);
        assert!(chain.is_canonical(2));
        assert!(chain.is_canonical(3));
        assert!(!chain.is_canonical(4));
        assert!(chain.is_canonical(5));
    }

    #[test]
    fn appends_across_bucket_boundaries() {
        let chain = CanonicalChain::new();
        for id in 1..=10_000 {
            chain.append(id);
        }

        assert!(chain.is_canonical(1));
        assert!(chain.is_canonical(4_096));
        assert!(chain.is_canonical(4_097));
        assert!(chain.is_canonical(10_000));
        assert!(!chain.is_canonical(10_001));
    }

    #[test]
    fn deep_rollback_rewrites_a_sealed_bucket() {
        let chain = CanonicalChain::new();
        for id in 1..=10_000 {
            chain.append(id);
        }
        assert!(chain.is_canonical(100));

        chain.rollback(50);

        assert_eq!(chain.tip(), 50);
        assert!(chain.is_canonical(50));
        assert!(!chain.is_canonical(51));
        assert!(!chain.is_canonical(100));
        assert!(!chain.is_canonical(10_000));
    }

    #[test]
    fn finalize_makes_pruned_gaps_read_canonical() {
        let chain = CanonicalChain::new();
        for id in 1..=10_000 {
            chain.append(id);
        }

        // Roll back deep, then extend past the gap so 100 stays orphaned below the new tip.
        chain.rollback(50);
        chain.append(10_001);
        assert!(!chain.is_canonical(100), "100 is orphaned in the gap");

        // Finalizing past the gap prunes its bucket, which then reads as canonical.
        chain.finalize(8_200);
        assert!(chain.is_canonical(100), "pruned bucket reads as finalized-canonical");
    }

    #[test]
    fn snapshot_is_stable_across_a_shallow_rollback() {
        let chain = CanonicalChain::new();
        chain.append(1);
        chain.append(2);
        chain.append(3);

        let snap = chain.snapshot();
        chain.rollback(1);

        assert!(!chain.is_canonical(3), "live chain reflects the rollback");
        assert!(snap.is_canonical(3), "snapshot keeps its perception");
        assert_eq!(snap.tip(), 3);
    }

    #[test]
    fn snapshot_is_stable_across_a_deep_rollback() {
        let chain = CanonicalChain::new();
        for id in 1..=10_000 {
            chain.append(id);
        }

        let snap = chain.snapshot();
        chain.rollback(50);

        assert!(!chain.is_canonical(100), "live chain reflects the rollback");
        assert!(snap.is_canonical(100), "snapshot keeps its perception of the sealed bucket");
    }

    #[test]
    fn concurrent_reads_during_writes_are_safe() {
        let chain = CanonicalChain::new();
        chain.append(1);

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let chain = chain.clone();
                thread::spawn(move || {
                    for _ in 0..10_000 {
                        let _ = chain.is_canonical(1);
                        let _ = chain.is_canonical(chain.tip());
                        let _ = chain.snapshot();
                    }
                })
            })
            .collect();

        for id in 2..=8_000 {
            chain.append(id);
        }
        for r in readers {
            r.join().unwrap();
        }

        assert_eq!(chain.tip(), 8_000);
        assert!(chain.is_canonical(8_000));
    }

    #[test]
    fn snapshot_shared_across_threads() {
        let chain = CanonicalChain::new();
        chain.append(1);
        chain.append(2);

        let snap = chain.snapshot();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let snap: Arc<_> = Arc::clone(&snap);
                thread::spawn(move || {
                    assert!(snap.is_canonical(1));
                    assert!(snap.is_canonical(2));
                    assert!(!snap.is_canonical(3));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
