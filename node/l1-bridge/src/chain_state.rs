use std::sync::Arc;

use kaspa_hashes::Hash as BlockHash;

use crate::coordinate::{ChainCoordinate, ChainCoordinateData};

/// Chain state tracking using a doubly-linked list of coordinates.
///
/// Supports O(1) append, O(n) rollback, and O(n) hash lookup by walking the list.
/// `last_pruned` represents the finalized/pruning threshold, `last_processed` is the latest
/// processed block.
pub struct ChainState {
    /// Finalized/pruning threshold (oldest block we track).
    last_pruned: Option<Arc<ChainCoordinateData>>,
    /// Latest processed block.
    last_processed: Option<Arc<ChainCoordinateData>>,
}

impl ChainState {
    /// Creates a new chain state, optionally starting from a known coordinate.
    ///
    /// Both pointers start at the given coordinate. The caller advances `last_processed`
    /// by calling [`add_block`].
    pub fn new(starting_coordinate: Option<ChainCoordinate>) -> Self {
        match starting_coordinate {
            Some(coord) => {
                Self { last_pruned: Some(coord.0.clone()), last_processed: Some(coord.0) }
            }
            None => Self { last_pruned: None, last_processed: None },
        }
    }

    /// Returns the last processed coordinate.
    pub fn last_processed(&self) -> Option<ChainCoordinate> {
        self.last_processed.clone().map(ChainCoordinate)
    }

    /// Returns the last pruned coordinate.
    pub fn last_pruned(&self) -> Option<ChainCoordinate> {
        self.last_pruned.clone().map(ChainCoordinate)
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
            self.last_pruned = Some(coord.0.clone());
        }

        self.last_processed = Some(coord.0.clone());
        index
    }

    /// Rolls back by the given number of blocks, returning the new index.
    ///
    /// Returns `Ok(index)` on success, or `Err` if the rollback would go past `last_pruned`.
    pub fn rollback(&mut self, num_blocks: u64) -> Result<u64, ()> {
        let pruned_index = self.last_pruned.as_ref().map(|c| c.index()).unwrap_or(0);
        let mut current = self.last_processed();
        let mut remaining = num_blocks;

        // Walk back n nodes, unlinking each removed node to break Arc cycles.
        while remaining > 0 && current.is_some() {
            let node = current.unwrap();
            if node.index() <= pruned_index {
                return Err(());
            }
            let prev = node.prev();
            node.clear_prev();
            node.set_next(None);
            current = prev;
            remaining -= 1;
        }

        // Update last_processed.
        if let Some(new_tail) = &current {
            new_tail.set_next(None);
            self.last_processed = Some(new_tail.0.clone());
        } else {
            self.last_processed = None;
            self.last_pruned = None;
        }

        Ok(current.map(|c| c.index()).unwrap_or(0))
    }

    /// Tries to advance finalization to the given hash.
    ///
    /// Returns `Ok(Some(coord))` if finalization advanced, `Ok(None)` if the hash matches the
    /// current `last_pruned` (no-op), or `Err` if the hash was not found in the chain.
    ///
    /// This walks forward from `last_pruned`, unlinking each node it passes. If the hash is
    /// not found, the chain is destroyed and the bridge must stop.
    pub fn try_finalize(&mut self, hash: &BlockHash) -> Result<Option<ChainCoordinate>, ()> {
        let current_pruned = match &self.last_pruned {
            Some(arc) => ChainCoordinate(arc.clone()),
            None => return Ok(None),
        };

        // Already at this pruning point.
        if current_pruned.hash() == *hash {
            return Ok(None);
        }

        // Walk forward, unlinking each node we pass.
        let mut current = current_pruned.next();
        current_pruned.clear_prev();
        current_pruned.set_next(None);

        while let Some(coord) = current {
            if coord.hash() == *hash {
                coord.clear_prev();
                self.last_pruned = Some(coord.0.clone());
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
