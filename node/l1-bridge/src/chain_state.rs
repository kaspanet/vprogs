use kaspa_hashes::Hash as BlockHash;

use crate::coordinate::ChainCoordinate;

/// Chain state tracking using a doubly-linked list of coordinates.
///
/// Supports O(1) append, O(n) rollback, and O(n) hash lookup by walking the list.
/// `last_pruned` represents the finalized/pruning threshold, `last_processed` is the latest
/// processed block. Both always point to a valid coordinate — on a fresh start, they point
/// to a sentinel root at index 0.
pub struct ChainState {
    /// Finalized/pruning threshold (oldest block we track).
    last_pruned: ChainCoordinate,
    /// Latest processed block.
    last_processed: ChainCoordinate,
}

impl ChainState {
    /// Creates a new chain state rooted at the given coordinate.
    ///
    /// Both pointers start at the root. The caller advances `last_processed`
    /// by calling [`add_block`].
    pub fn new(root: ChainCoordinate) -> Self {
        Self { last_pruned: root.clone(), last_processed: root }
    }

    /// Returns the last processed coordinate.
    pub fn last_processed(&self) -> ChainCoordinate {
        self.last_processed.clone()
    }

    /// Returns the last pruned coordinate.
    pub fn last_pruned(&self) -> ChainCoordinate {
        self.last_pruned.clone()
    }

    /// Allocates the next index and records the block.
    pub fn add_block(&mut self, hash: BlockHash) -> u64 {
        let index = self.last_processed.index() + 1;
        let coord = ChainCoordinate::new_linked(hash, index, Some(self.last_processed.clone()));
        self.last_processed.set_next(Some(coord.clone()));
        self.last_processed = coord;
        index
    }

    /// Rolls back by the given number of blocks, returning the new index.
    ///
    /// Returns `Ok(index)` on success, or `Err` if the rollback would go past `last_pruned`.
    pub fn rollback(&mut self, num_blocks: u64) -> Result<u64, ()> {
        let pruned_index = self.last_pruned.index();

        for _ in 0..num_blocks {
            if self.last_processed.index() <= pruned_index {
                return Err(());
            }
            let prev = self.last_processed.prev().expect("non-root node must have prev");
            self.last_processed.clear_prev();
            prev.set_next(None);
            self.last_processed = prev;
        }

        Ok(self.last_processed.index())
    }

    /// Tries to advance finalization to the given hash.
    ///
    /// Returns `Ok(Some(coord))` if finalization advanced, `Ok(None)` if the hash matches the
    /// current `last_pruned` (no-op), or `Err` if the hash was not found in the chain.
    ///
    /// This walks forward from `last_pruned`, unlinking each node it passes. If the hash is
    /// not found, the chain is destroyed and the bridge must stop.
    pub fn try_finalize(&mut self, hash: &BlockHash) -> Result<Option<ChainCoordinate>, ()> {
        // Already at this pruning point.
        if self.last_pruned.hash() == *hash {
            return Ok(None);
        }

        // Walk forward, unlinking each node we pass.
        let mut current = self.last_pruned.next();
        self.last_pruned.clear_prev();
        self.last_pruned.set_next(None);

        while let Some(coord) = current {
            if coord.hash() == *hash {
                coord.clear_prev();
                self.last_pruned = coord.clone();
                return Ok(Some(coord));
            }
            let next = coord.next();
            coord.clear_prev();
            coord.set_next(None);
            current = next;
        }

        // Hash not found — chain is destroyed.
        Err(())
    }
}
