//! The immutable read side of the canonical chain.
//!
//! # Bit layout
//!
//! Every L2 batch owns a dense, 1-based, never-reused id, and the chain stores one bit per id:
//! `1` means canonical (on the tip's ancestry) and `0` means orphaned. Id `0` is the "no parent"
//! sentinel and is never canonical.
//!
//! Bits are grouped into fixed-size [`Bucket`]s of [`BUCKET_CAPACITY`](crate::BUCKET_CAPACITY)
//! consecutive ids: bucket `b` covers ids `b * CAPACITY + 1 ..= (b + 1) * CAPACITY`. A snapshot
//! splits those buckets into three regions, hottest on the right (`T` is `tail_bucket`, the highest
//! allocated bucket):
//!
//! ```text
//!   bucket:    0 ........... k-1 | k ........... T-2 | T-1 ......... T
//!              \__ finalized __/   \____ body ____/    \_ hot zone _/
//!               (pruned away)       (sealed ring)       last_sealed, tail
//!   read:       always canonical    the stored bit      the stored bit, set
//!               (bucket absent)     (true if pruned)    in place by the writer
//! ```
//!
//! * **Hot zone**: the top two buckets, held directly as `Arc<Bucket>`. `tail` (bucket `T`) is the
//!   one currently filling; `last_sealed` (bucket `T - 1`) is kept hot so any reorg that stays
//!   within the last two buckets touches only these and never the body ring. See [`HotZone`].
//! * **Body**: buckets `0 ..= T - 2`, sealed into an [`AtomicRing`]. A reorg deep enough to rewrite
//!   a sealed bucket forks the ring copy-on-write, so snapshots published earlier keep their view.
//! * **Finalized**: buckets below the body's base, pruned by `finalize` once their ids can no
//!   longer reorg. An absent bucket reads canonical for *every* id in it, orphaned ids included;
//!   this is sound only if callers never query below the finalization threshold.
//!
//! For example, with `CAPACITY == 128` and `tip == 300`, id 300 sits at bucket 2 bit 43: `tail` is
//! bucket 2 (ids 257..=384), `last_sealed` is bucket 1 (ids 129..=256), and the body holds bucket 0
//! (ids 1..=128).

use std::sync::Arc;

use vprogs_core_atomics::AtomicRing;

use crate::{
    bucket::{Bucket, BucketWords},
    hot_zone::HotZone,
};

/// A snapshot of the canonical bits, taken at publication.
///
/// A snapshot's answers are stable where it matters: for any id at or above the finalization
/// threshold, [`is_canonical`](Self::is_canonical) returns the same value for as long as the
/// snapshot is held - a canonical bit never flips. Below the threshold there is no such guarantee -
/// a later `finalize` prunes those buckets out of the body ring this snapshot shares, after which
/// every id in them reads canonical regardless of its real bit - so callers must not query below
/// the finalization threshold.
pub struct CanonicalChainSnapshot {
    /// Highest canonical id and the upper bound for reads. `0` means empty.
    pub(crate) tip: u64,
    /// Highest id ever assigned, monotonic across rollbacks; `0` means no id was ever assigned.
    pub(crate) high_water: u64,
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

        // Read the stored bit; an absent (pruned-off, finalized) bucket reads canonical.
        let (bucket, bit) = Bucket::locate(id);
        self.read_bucket(bucket, |sealed| sealed.get(bit)).unwrap_or(true)
    }

    /// Returns the bucket's raw words, or `None` if the bucket sits below the body's base
    /// (already pruned) or above the allocated hot zone.
    pub fn bucket_words(&self, bucket: u64) -> Option<BucketWords> {
        self.read_bucket(bucket, |sealed| sealed.words())
    }

    /// Reads through the bucket holding `bucket`'s bits: a hot-zone bucket directly, else the
    /// body's sealed copy; `None` below the body's base.
    fn read_bucket<R>(&self, bucket: u64, read: impl FnOnce(&Bucket) -> R) -> Option<R> {
        let hot = &self.hot_zone;
        if bucket == hot.tail_bucket {
            return Some(read(&hot.tail));
        }
        if hot.tail_bucket >= 1 && bucket == hot.tail_bucket - 1 {
            return Some(read(&hot.last_sealed));
        }
        self.body.with(bucket, |sealed| read(sealed))
    }

    /// Returns the highest canonical id (the read bound).
    pub fn tip(&self) -> u64 {
        self.tip
    }

    /// Returns whether the chain has ever assigned an id; unlike `tip`, this never reverts.
    pub fn ever_assigned(&self) -> bool {
        self.high_water > 0
    }

    /// The empty view: no canonical ids.
    pub(crate) fn empty() -> Self {
        Self {
            tip: 0,
            high_water: 0,
            hot_zone: HotZone::empty(),
            body: Arc::new(AtomicRing::new(0)),
        }
    }
}
