use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::{ArcSwap, ArcSwapOption};

/// Lock-free, growable, index-addressed rolling buffer.
pub struct AtomicRing<V> {
    /// Index of the oldest live entry.
    base: AtomicU64,
    /// One past the highest live index.
    tip: AtomicU64,
    /// Slot array; only this is replaced on growth. An index maps to `index % len`.
    slots: ArcSwap<Box<[Slot<V>]>>,
}

impl<V> AtomicRing<V> {
    /// Initial slot count, before the first growth.
    const DEFAULT_CAPACITY: usize = 64;

    /// Creates an empty ring whose first [`push`](Self::push) assigns `base`.
    pub fn new(base: u64) -> Self {
        Self::with_capacity(base, Self::DEFAULT_CAPACITY)
    }

    /// Creates an empty ring with room for `capacity` live entries before the first growth.
    pub fn with_capacity(base: u64, capacity: usize) -> Self {
        Self {
            base: AtomicU64::new(base),
            tip: AtomicU64::new(base),
            slots: ArcSwap::from_pointee(Self::empty_slots(capacity)),
        }
    }

    /// Returns the index the next [`push`](Self::push) will assign.
    pub fn next_index(&self) -> u64 {
        self.tip.load(Ordering::Acquire)
    }

    /// Returns the lowest live index (the pruning floor).
    pub fn base(&self) -> u64 {
        self.base.load(Ordering::Acquire)
    }

    /// Appends `value` at the next index and returns it, growing if full. Single-writer.
    pub fn push(&self, value: V) -> u64 {
        // The value takes the next index; load the array it will be stored in.
        let index = self.next_index();
        let slots = self.slots.load();

        // Increase the size if the new index would wrap around and overwrite a live entry.
        if index - self.base() >= slots.len() as u64 {
            drop(slots);
            self.grow();
            return self.push(value);
        }

        // Publish the slot before the tip so a reader seeing the new tip also sees the slot.
        Self::slot(&slots, index).store(Some(Arc::new((index, value))));
        self.tip.store(index + 1, Ordering::Release);

        index
    }

    /// Reads the value at `index` through `read`, or returns `None` if outside the live range.
    pub fn with<R>(&self, index: u64, read: impl FnOnce(&V) -> R) -> Option<R> {
        // Abort early if the index is out of bounds.
        if index < self.base() || index >= self.next_index() {
            return None;
        }

        // The stored-index check rejects a slot a wrapping append reused for `index + capacity`.
        match Self::slot(&self.slots.load(), index).load_full() {
            Some(entry) if entry.0 == index => Some(read(&entry.1)),
            _ => None,
        }
    }

    /// Returns a clone of the value at `index`, or `None` if outside the live range.
    pub fn get(&self, index: u64) -> Option<V>
    where
        V: Clone,
    {
        self.with(index, V::clone)
    }

    /// Drops every entry at index `>= from` (truncating the suffix). Single-writer.
    pub fn truncate_from(&self, from: u64) {
        // Clamp so truncation never raises the tip or drops below the live base.
        let from = from.max(self.base()).min(self.next_index());
        self.tip.store(from, Ordering::Release);
    }

    /// Drops every entry at index `< below` (pruning the prefix). Single-writer.
    pub fn prune_below(&self, below: u64) {
        // Clamp so the base only ever advances and never passes the tip.
        let below = below.max(self.base()).min(self.next_index());
        self.base.store(below, Ordering::Release);
    }

    /// Replaces the slot array with a double-capacity one, re-placing live entries. Single-writer.
    fn grow(&self) {
        // Allocate a fresh array with double the capacity.
        let old = self.slots.load_full();
        let new = Self::empty_slots(old.len() * 2);

        // Re-place each live entry into the larger array, skipping slots a prior wrap left stale.
        for index in self.base()..self.next_index() {
            if let Some(entry) = Self::slot(&old, index).load_full() {
                if entry.0 == index {
                    Self::slot(&new, index).store(Some(entry));
                }
            }
        }

        // Publish the new array so later loads and pushes use it.
        self.slots.store(Arc::new(new));
    }

    /// Builds `capacity` empty slots.
    fn empty_slots(capacity: usize) -> Box<[Slot<V>]> {
        assert!(capacity > 0, "capacity must be non-zero");
        (0..capacity).map(|_| ArcSwapOption::empty()).collect()
    }

    /// The slot a given index maps to within `slots`.
    fn slot(slots: &[Slot<V>], index: u64) -> &Slot<V> {
        &slots[(index % slots.len() as u64) as usize]
    }
}

/// A ring slot: the `(index, value)` last stored here, or empty.
type Slot<V> = ArcSwapOption<(u64, V)>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_reflect_writes() {
        let ring = AtomicRing::new(1);
        assert_eq!(ring.push(10u64), 1);
        assert_eq!(ring.push(20), 2);
        assert_eq!(ring.get(1), Some(10));
        assert_eq!(ring.get(2), Some(20));
        assert_eq!(ring.get(3), None);
    }

    #[test]
    fn truncate_drops_the_suffix() {
        let ring = AtomicRing::new(1);
        for v in 1..=3u64 {
            ring.push(v);
        }
        ring.truncate_from(2);
        assert_eq!(ring.next_index(), 2);
        assert_eq!(ring.get(2), None);
        assert_eq!(ring.push(99), 2); // index 2 reused by the new value
        assert_eq!(ring.get(2), Some(99));
    }

    #[test]
    fn prune_drops_the_prefix() {
        let ring = AtomicRing::new(1);
        for v in 1..=4u64 {
            ring.push(v);
        }
        ring.prune_below(3);
        assert_eq!(ring.get(2), None);
        assert_eq!(ring.get(3), Some(3));
    }

    #[test]
    fn wraps_after_pruning() {
        let ring = AtomicRing::with_capacity(1, 2);
        ring.push(1u64); // index 1 -> slot 1
        ring.push(2); // index 2 -> slot 0 (window full: {1, 2})
        ring.prune_below(2); // window {2}
        assert_eq!(ring.push(3), 3); // index 3 -> slot 1, reusing index 1's slot

        assert_eq!(ring.get(1), None);
        assert_eq!(ring.get(2), Some(2));
        assert_eq!(ring.get(3), Some(3));
    }

    #[test]
    fn grows_and_remaps_when_capacity_exceeded() {
        // Start at capacity 2 and push past it without pruning: the ring doubles and re-places
        // every live entry instead of panicking.
        let ring = AtomicRing::with_capacity(1, 2);
        for v in 1..=5u64 {
            assert_eq!(ring.push(v), v);
        }
        for v in 1..=5u64 {
            assert_eq!(ring.get(v), Some(v));
        }
    }

    #[test]
    fn with_reads_without_requiring_clone() {
        // A deliberately non-`Clone` value: `with` borrows it rather than cloning it out.
        struct NotClone(u64);
        let ring = AtomicRing::new(1);
        ring.push(NotClone(7));
        assert_eq!(ring.with(1, |v| v.0), Some(7));
        assert_eq!(ring.with(2, |v| v.0), None);
    }
}
