//! The single-writer build side of the canonical chain.
//!
//! Each mutation builds a brand-new [`CanonicalChainSnapshot`] and atomically publishes it, so a
//! reader holding an older snapshot keeps a stable view. See [`snapshot`](crate::snapshot) for the
//! bit layout (hot zone, body, finalized) that these operations maintain. In those terms:
//!
//! * [`append`](CanonicalChain::append) sets the next bit in the `tail` bucket. When the new id
//!   crosses into a fresh bucket the hot zone slides up by one: the old `tail` becomes the new
//!   `last_sealed`, the old `last_sealed` is sealed into the body ring, and a fresh empty `tail` is
//!   allocated (the sealing is done by [`HotZone::roll_forward`]).
//! * [`rollback`](CanonicalChain::rollback) clears every bit above `new_tip`. A shallow rollback
//!   rewrites only the copy-on-write hot buckets; a deep one that reaches a sealed bucket forks the
//!   body ring first, so earlier snapshots are untouched.
//! * [`finalize`](CanonicalChain::finalize) prunes the body buckets below the finalized id; once
//!   pruned, those buckets read as canonical. [`frozen_bits`](CanonicalChain::frozen_bits) captures
//!   their words before the prune, so a restart can persist and replay them.
//! * [`restore`](CanonicalChain::restore) rebuilds the whole layout from persisted ids at startup.
//!
//! One visibility wrinkle in the "older snapshots stay stable" guarantee, unique to `append`: it is
//! the only operation that mutates a bucket in place (`tail.set`), on a bucket the previous
//! snapshot may still share, so a reader on that older snapshot can observe the bit early. That is
//! benign: the bit belongs to an `id` above the old snapshot's `tip`, which
//! [`is_canonical`](CanonicalChainSnapshot::is_canonical) rejects before reading any bit. The other
//! mutators never expose a bit early - `rollback` edits copy-on-write (fresh buckets via
//! [`Bucket::edited`]), and `finalize` only prunes.

use std::{
    collections::BTreeMap,
    iter::repeat_with,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use arc_swap::ArcSwap;
use vprogs_core_atomics::AtomicRing;

use crate::{
    bucket::{Bucket, FrozenBits, WORDS, sub_base_words},
    hot_zone::HotZone,
    snapshot::CanonicalChainSnapshot,
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

    /// Returns the canonical bits frozen below `below`: full words for every bucket the range
    /// fully crosses, plus the sub-base words of the bucket holding `below`. Valid while the
    /// buckets are still live, before [`finalize`](Self::finalize) prunes them.
    pub fn frozen_bits(&self, below: u64) -> Vec<FrozenBits> {
        // Read the current snapshot; crossed buckets must still be present in it.
        let cur = self.current.load();
        let (base_bucket, base_bit) = Bucket::locate(below);

        // Full words for each bucket the step crosses below the new base bucket.
        let mut frozen = Vec::new();
        for bucket in cur.body.base()..base_bucket {
            if let Some(words) = cur.bucket_words(bucket) {
                frozen.push(FrozenBits { bucket, words });
            }
        }

        // The sub-base range of the new base bucket: masked words, so still-live bits stay out.
        if let Some(words) = cur.bucket_words(base_bucket) {
            frozen.push(FrozenBits { bucket: base_bucket, words: sub_base_words(words, base_bit) });
        }
        frozen
    }

    /// Restores the `canonical` ids over `base..=tip` at startup, replaying `frozen` bits
    /// below `base`. `last_assigned` names the highest id the log ever allocated, including
    /// entries orphaned above `tip`. Ids whose bits were never persisted read canonical, like
    /// the pruned-bucket fallback.
    pub(crate) fn restore(
        &self,
        base: u64,
        tip: u64,
        last_assigned: u64,
        canonical: impl IntoIterator<Item = u64>,
        frozen: &[FrozenBits],
    ) {
        // Nothing to restore for an empty chain.
        if tip == 0 {
            return;
        }

        // Bucket range spanned by the live ids.
        let base_bucket = Bucket::locate(base).0;
        let tail_bucket = Bucket::locate(tip).0;

        // One owned bucket per live bucket number, with the canonical bits set in place.
        let bucket_count = (tail_bucket - base_bucket + 1) as usize;
        let mut buckets: Vec<Bucket> = repeat_with(Bucket::new).take(bucket_count).collect();
        for id in canonical {
            let (bucket, bit) = Bucket::locate(id);
            buckets[(bucket - base_bucket) as usize].set(bit);
        }

        // The base bucket spans ids below `base` too, where the persisted log carries no bits;
        // a live chain finalized to `base` keeps that bucket, so replay its persisted words
        // (or read canonical where none survived) instead of leaving the range orphaned.
        let (_, base_bit) = Bucket::locate(base);
        let seed = frozen
            .iter()
            .find(|row| row.bucket == base_bucket)
            .map_or([u64::MAX; WORDS], |row| row.words);
        for bit in 0..base_bit {
            if seed[bit / 64] & (1 << (bit % 64)) != 0 {
                buckets[0].set(bit);
            }
        }

        // Peel the hot zone off the top; seal the rest into a ring at the live floor.
        let tail = Arc::new(buckets.pop().expect("live range has at least one bucket"));

        // A single-bucket live range leaves nothing to seal; the fabricated `last_sealed` covers
        // ids entirely below `base`, so it replays that bucket's persisted words, reading
        // canonical throughout where none survived.
        let fabricated = base_bucket
            .checked_sub(1)
            .and_then(|below| frozen.iter().find(|row| row.bucket == below))
            .map_or_else(Bucket::all_canonical, |row| Bucket::from_words(row.words));
        let last_sealed = buckets.pop().map_or_else(|| Arc::new(fabricated), Arc::new);
        let body = AtomicRing::new(base_bucket);
        for bucket in buckets {
            body.push(Arc::new(bucket));
        }

        // Publish the restored snapshot; ids orphaned above the tip stay assigned.
        self.current.store(Arc::new(CanonicalChainSnapshot {
            tip,
            high_water: last_assigned,
            hot_zone: HotZone { tail_bucket, tail, last_sealed },
            body: Arc::new(body),
        }));
    }

    /// Restores a chain rolled back to genesis with every persisted entry orphaned, so reads
    /// keep filtering those ids.
    pub(crate) fn restore_genesis(&self, last_assigned: u64) {
        // The high water still names the highest id ever assigned; tip 0 reads nothing canonical.
        self.current.store(Arc::new(CanonicalChainSnapshot {
            tip: 0,
            high_water: last_assigned,
            hot_zone: HotZone::empty(),
            body: Arc::new(AtomicRing::new(0)),
        }));
    }

    /// Marks `id` canonical as the new tip. Debug-panics unless `id` > tip.
    pub(crate) fn append(&self, id: u64) {
        // Read the current snapshot; the new id must extend its tip.
        let cur = self.current.load();
        debug_assert!(id > cur.tip, "append id {id} must extend tip {}", cur.tip);

        // Append - advance hot zone, reuse shared ring and set tail bit in place, no copy.
        let (bucket, bit) = Bucket::locate(id);
        let body = Arc::clone(&cur.body);
        let hot_zone = cur.hot_zone.roll_forward(bucket, &body);
        hot_zone.tail.set(bit);

        // Publish the extended snapshot.
        self.current.store(Arc::new(CanonicalChainSnapshot {
            tip: id,
            high_water: id.max(cur.high_water),
            hot_zone,
            body,
        }));
    }

    /// Rolls the chain back to `new_tip`, flipping off every id above it.
    pub(crate) fn rollback(&self, new_tip: u64) {
        // Read the current snapshot; the target must not exceed its tip.
        let cur = self.current.load();
        debug_assert!(new_tip <= cur.tip, "rollback target {new_tip} exceeds tip {}", cur.tip);

        // Fork the body only for a deep rollback that rewrites a sealed bucket; else share it.
        let deep = Bucket::locate(new_tip + 1).0 + 2 <= cur.hot_zone.tail_bucket;
        let body = if deep { Arc::new(cur.body.fork()) } else { Arc::clone(&cur.body) };
        let mut hot_zone = cur.hot_zone.roll_forward(cur.hot_zone.tail_bucket, &body);

        // Group the bit edits by bucket so each touched bucket is rewritten once.
        let mut edits: BTreeMap<u64, Vec<(usize, bool)>> = BTreeMap::new();
        for id in new_tip + 1..=cur.tip {
            let (bucket, bit) = Bucket::locate(id);
            edits.entry(bucket).or_default().push((bit, false));
        }

        // Hot-zone buckets copy-on-write; body buckets replace in the ring; pruned edits drop.
        for (bucket, ops) in edits {
            if bucket == hot_zone.tail_bucket {
                hot_zone.tail = hot_zone.tail.edited(&ops);
            } else if hot_zone.tail_bucket >= 1 && bucket == hot_zone.tail_bucket - 1 {
                hot_zone.last_sealed = hot_zone.last_sealed.edited(&ops);
            } else if let Some(sealed) = body.get(bucket) {
                body.replace(bucket, sealed.edited(&ops));
            }
        }

        // Publish the rolled-back snapshot; assigned ids stay assigned, so the high water holds.
        self.current.store(Arc::new(CanonicalChainSnapshot {
            tip: new_tip,
            high_water: cur.high_water,
            hot_zone,
            body,
        }));
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
