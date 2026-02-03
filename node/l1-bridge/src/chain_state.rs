use arc_swap::ArcSwapOption;
use kaspa_hashes::Hash as BlockHash;

use crate::coordinate::{ChainCoordinate, ChainCoordinateData};

/// Chain state tracking using a doubly-linked list of coordinates.
///
/// Supports O(1) append, O(n) rollback, and O(n) hash lookup by walking the list.
/// Head represents the finalized/pruning threshold, tail is the latest processed.
pub struct ChainState {
    /// Head of the list (finalized/pruning threshold).
    head: ArcSwapOption<ChainCoordinateData>,
    /// Tail of the list (latest processed).
    tail: ArcSwapOption<ChainCoordinateData>,
}

impl ChainState {
    /// Creates a new chain state.
    pub fn new(last_processed: Option<ChainCoordinate>) -> Self {
        let state = Self { head: ArcSwapOption::empty(), tail: ArcSwapOption::empty() };

        if let Some(coord) = last_processed {
            state.head.store(Some(coord.0.clone()));
            state.tail.store(Some(coord.0.clone()));
        }

        state
    }

    /// Returns the last processed coordinate.
    pub fn last_processed(&self) -> Option<ChainCoordinate> {
        self.tail.load_full().map(ChainCoordinate)
    }

    /// Allocates the next index and records the block.
    pub fn add_block(&mut self, hash: BlockHash) -> u64 {
        let prev = self.last_processed();
        let index = prev.as_ref().map(|c| c.index()).unwrap_or(0) + 1;

        let coord = ChainCoordinate::new_linked(hash, index, prev.clone());

        // Link previous coordinate's next to new one.
        if let Some(prev_coord) = prev {
            prev_coord.set_next(Some(coord.clone()));
        } else {
            // First coordinate - also set as head.
            self.head.store(Some(coord.0.clone()));
        }

        self.tail.store(Some(coord.0.clone()));
        index
    }

    /// Rolls back by the given number of blocks, returning the new index.
    pub fn rollback(&mut self, num_blocks: u64) -> u64 {
        let mut current = self.last_processed();
        let mut remaining = num_blocks;

        // Walk back n nodes.
        while remaining > 0 && current.is_some() {
            current = current.unwrap().prev();
            remaining -= 1;
        }

        // Update tail.
        if let Some(new_tail) = &current {
            new_tail.set_next(None);
            self.tail.store(Some(new_tail.0.clone()));
        } else {
            // Rolled back everything.
            self.tail.store(None);
            self.head.store(None);
        }

        current.map(|c| c.index()).unwrap_or(0)
    }

    /// Finds a coordinate by hash, walking forward from head.
    pub fn find_by_hash(&self, hash: &BlockHash) -> Option<ChainCoordinate> {
        let mut current = self.head.load_full().map(ChainCoordinate);
        while let Some(coord) = current {
            if coord.hash() == *hash {
                return Some(coord);
            }
            current = coord.next();
        }
        None
    }

    /// Returns the finalized coordinate (head of the list).
    pub fn finalized(&self) -> Option<ChainCoordinate> {
        self.head.load_full().map(ChainCoordinate)
    }

    /// Advances the finalized threshold and prunes old nodes.
    pub fn advance_finalized(&mut self, coord: &ChainCoordinate) {
        // Prune coordinates before this one by advancing head.
        let mut current = self.head.load_full().map(ChainCoordinate);
        while let Some(ref head) = current {
            if head.index() >= coord.index() {
                break;
            }
            current = head.next();
        }

        // Update head.
        if let Some(new_head) = current {
            new_head.clear_prev();
            self.head.store(Some(new_head.0));
        }
    }
}
