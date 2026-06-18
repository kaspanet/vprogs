use std::{ops::RangeInclusive, sync::Arc};

use vprogs_core_atomics::AtomicRing;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::{BatchMetadata, CanonicalChain, Checkpoint};
use vprogs_state_canonical_chain::CanonicalChain as CanonicalChainStore;
use vprogs_storage_types::{Store, WriteBatch};

/// Growable, cheaply-clonable [`CanonicalChain`] backed by a lock-free rolling ring.
#[smart_pointer]
pub struct LockFreeCanonicalChain {
    /// Maps each batch index to the canonical block hash recorded for it.
    ring: AtomicRing<[u8; 32]>,
}

impl LockFreeCanonicalChain {
    /// Creates an empty chain whose first [`append`](Self::append) assigns `base`.
    pub fn new(base: u64) -> Self {
        Self(Arc::new(LockFreeCanonicalChainData { ring: AtomicRing::new(base) }))
    }

    /// Rebuilds the chain from its persisted entries, or starts empty at `base`. Startup only.
    pub fn restore<S: Store>(store: &S, base: u64) -> Self {
        // Base the ring at the first stored index, or at `base` if the store is empty.
        let mut entries = CanonicalChainStore::iter(store).peekable();
        let Some(&(first, _)) = entries.peek() else {
            return Self::new(base);
        };

        // Replay the persisted entries in index order to rebuild the ring.
        let chain = Self::new(first);
        for (index, block_hash) in entries {
            let assigned = chain.ring.push(block_hash);
            debug_assert_eq!(assigned, index, "canonical chain is not contiguous at {index}");
        }

        chain
    }

    /// Returns the index the next [`append`](Self::append) will assign.
    pub fn next_index(&self) -> u64 {
        self.ring.next_index()
    }

    /// Appends `checkpoint`'s block hash as the next canonical batch in memory. Single-writer.
    pub fn append<M: BatchMetadata>(&self, checkpoint: &Checkpoint<M>) {
        let index = self.ring.push(checkpoint.metadata().block_hash());
        debug_assert_eq!(index, checkpoint.index(), "canonical chain diverged from the checkpoint");
    }

    /// Discards every in-memory batch at `index` and above (reorg). Single-writer.
    pub fn rollback_to(&self, index: u64) {
        self.ring.truncate_from(index);
    }

    /// Drops every in-memory batch at index `< below` (the finalized prefix). Single-writer.
    pub fn prune_below(&self, below: u64) {
        self.ring.prune_below(below);
    }

    /// Stages the durable canonical entry for `checkpoint`.
    pub fn append_to_disk<M, W>(&self, wb: &mut W, checkpoint: &Checkpoint<M>)
    where
        M: BatchMetadata,
        W: WriteBatch,
    {
        CanonicalChainStore::set(wb, checkpoint.index(), &checkpoint.metadata().block_hash());
    }

    /// Stages the durable deletes for every batch index in `range`.
    pub fn delete_from_disk<W: WriteBatch>(&self, wb: &mut W, range: RangeInclusive<u64>) {
        for index in range {
            CanonicalChainStore::delete(wb, index);
        }
    }
}

impl Default for LockFreeCanonicalChain {
    fn default() -> Self {
        Self::new(0)
    }
}

impl CanonicalChain for LockFreeCanonicalChain {
    fn block(&self, index: u64) -> Option<[u8; 32]> {
        self.ring.get(index)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
    use vprogs_storage_types::Store;

    use super::*;

    /// Test checkpoint; the `u64` metadata's hash is its zero-padded bytes.
    fn cp(index: u64, metadata: u64) -> Checkpoint<u64> {
        Checkpoint::new(index, metadata)
    }

    fn store() -> (TempDir, RocksDbStore<DefaultConfig>) {
        let dir = TempDir::new().expect("failed to create temp dir");
        let store = RocksDbStore::<DefaultConfig>::open(dir.path());
        (dir, store)
    }

    /// Mirrors the scheduler: append in memory, then stage and commit the durable write.
    fn commit_block(
        chain: &LockFreeCanonicalChain,
        store: &RocksDbStore<DefaultConfig>,
        checkpoint: &Checkpoint<u64>,
    ) {
        chain.append(checkpoint);
        let mut wb = store.write_batch();
        chain.append_to_disk(&mut wb, checkpoint);
        store.commit(wb);
    }

    #[test]
    fn appends_read_back_in_memory() {
        let (_dir, store) = store();
        let chain = LockFreeCanonicalChain::new(1);
        commit_block(&chain, &store, &cp(1, 1));
        commit_block(&chain, &store, &cp(2, 2));

        assert!(chain.is_canonical(1, &1u64.block_hash()));
        assert!(!chain.is_canonical(1, &2u64.block_hash()));
        assert_eq!(chain.block(2), Some(2u64.block_hash()));
        assert_eq!(chain.block(3), None);
    }

    #[test]
    fn restore_round_trips_through_storage() {
        let (_dir, store) = store();
        let chain = LockFreeCanonicalChain::new(1);
        for b in 1..=3 {
            commit_block(&chain, &store, &cp(b, b));
        }

        let restored = LockFreeCanonicalChain::restore(&store, 1);
        assert_eq!(restored.next_index(), 4);
        assert!(restored.is_canonical(1, &1u64.block_hash()));
        assert!(restored.is_canonical(3, &3u64.block_hash()));
        assert_eq!(restored.block(4), None);
    }

    #[test]
    fn rollback_reassigns_and_clears_storage() {
        let (_dir, store) = store();
        let chain = LockFreeCanonicalChain::new(1);
        for b in 1..=3 {
            commit_block(&chain, &store, &cp(b, b));
        }

        // Reorg: roll back to index 2 (discard 2, 3), then append a different branch there.
        chain.rollback_to(2);
        let mut wb = store.write_batch();
        chain.delete_from_disk(&mut wb, 2..=3);
        store.commit(wb);
        commit_block(&chain, &store, &cp(2, 0x20));

        assert!(chain.is_canonical(2, &0x20u64.block_hash()));
        assert!(!chain.is_canonical(2, &2u64.block_hash()));

        let restored = LockFreeCanonicalChain::restore(&store, 1);
        assert!(restored.is_canonical(2, &0x20u64.block_hash()));
        assert_eq!(restored.next_index(), 3);
        assert_eq!(restored.block(3), None);
    }

    #[test]
    fn prune_drops_the_finalized_prefix_from_storage() {
        let (_dir, store) = store();
        let chain = LockFreeCanonicalChain::new(1);
        for b in 1..=4 {
            commit_block(&chain, &store, &cp(b, b));
        }

        chain.prune_below(3);
        let mut wb = store.write_batch();
        chain.delete_from_disk(&mut wb, 1..=2);
        store.commit(wb);

        assert_eq!(chain.block(2), None);
        assert!(chain.is_canonical(3, &3u64.block_hash()));

        let restored = LockFreeCanonicalChain::restore(&store, 1);
        assert_eq!(restored.block(2), None);
        assert!(restored.is_canonical(3, &3u64.block_hash()));
        assert_eq!(restored.next_index(), 5);
    }

    #[test]
    fn clones_share_one_canonical_view() {
        let (_dir, store) = store();
        let chain = LockFreeCanonicalChain::new(1);
        let view = chain.clone();
        commit_block(&chain, &store, &cp(1, 1));
        assert!(view.is_canonical(1, &1u64.block_hash()));
    }
}
