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

    /// Tries to advance finalization to the given hash.
    /// Returns the coordinate if it was found and is past the current head, None otherwise.
    pub fn try_finalize(&mut self, hash: &BlockHash) -> Option<ChainCoordinate> {
        let head_index = self.head.load_full().map(|h| h.index()).unwrap_or(0);

        // Walk forward from head looking for the hash.
        let mut current = self.head.load_full().map(ChainCoordinate);
        while let Some(coord) = current {
            if coord.hash() == *hash {
                // Found it - only advance if past current head.
                if coord.index() > head_index {
                    coord.clear_prev();
                    self.head.store(Some(coord.0.clone()));
                    return Some(coord);
                }
                return None;
            }
            current = coord.next();
        }
        None
    }
}
