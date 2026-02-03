use arc_swap::ArcSwapOption;
use kaspa_hashes::Hash as BlockHash;

use crate::coordinate::{ChainCoordinate, ChainCoordinateData};

/// Chain state tracking using a doubly-linked list of coordinates.
///
/// Supports O(1) append, O(n) rollback, and O(n) hash lookup by walking the list.
/// `last_pruned` represents the finalized/pruning threshold, `last_processed` is the latest
/// processed block.
pub struct ChainState {
    /// Finalized/pruning threshold (oldest block we track).
    last_pruned: ArcSwapOption<ChainCoordinateData>,
    /// Latest processed block.
    last_processed: ArcSwapOption<ChainCoordinateData>,
}

impl ChainState {
    /// Creates a new chain state, optionally starting from a known coordinate.
    ///
    /// Both pointers start at the given coordinate. The caller advances `last_processed`
    /// by calling [`add_block`].
    pub fn new(starting_coordinate: Option<ChainCoordinate>) -> Self {
        let state =
            Self { last_pruned: ArcSwapOption::empty(), last_processed: ArcSwapOption::empty() };

        if let Some(coord) = starting_coordinate {
            state.last_pruned.store(Some(coord.0.clone()));
            state.last_processed.store(Some(coord.0));
        }

        state
    }

    /// Returns the last processed coordinate.
    pub fn last_processed(&self) -> Option<ChainCoordinate> {
        self.last_processed.load_full().map(ChainCoordinate)
    }

    /// Returns the last pruned coordinate.
    pub fn last_pruned(&self) -> Option<ChainCoordinate> {
        self.last_pruned.load_full().map(ChainCoordinate)
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
            // First coordinate - also set as last_pruned.
            self.last_pruned.store(Some(coord.0.clone()));
        }

        self.last_processed.store(Some(coord.0.clone()));
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

        // Update last_processed.
        if let Some(new_tail) = &current {
            new_tail.set_next(None);
            self.last_processed.store(Some(new_tail.0.clone()));
        } else {
            // Rolled back everything.
            self.last_processed.store(None);
            self.last_pruned.store(None);
        }

        current.map(|c| c.index()).unwrap_or(0)
    }

    /// Tries to advance finalization to the given hash.
    /// Returns the coordinate if it was found and is past `last_pruned`, None otherwise.
    pub fn try_finalize(&mut self, hash: &BlockHash) -> Option<ChainCoordinate> {
        let pruned_index = self.last_pruned.load_full().map(|h| h.index()).unwrap_or(0);

        // Walk forward from last_pruned looking for the hash.
        let mut current = self.last_pruned.load_full().map(ChainCoordinate);
        while let Some(coord) = current {
            if coord.hash() == *hash {
                // Found it - only advance if past current last_pruned.
                if coord.index() > pruned_index {
                    coord.clear_prev();
                    self.last_pruned.store(Some(coord.0.clone()));
                    return Some(coord);
                }
                return None;
            }
            current = coord.next();
        }
        None
    }
}
