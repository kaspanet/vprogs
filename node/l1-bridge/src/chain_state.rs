use std::collections::HashMap;

use kaspa_hashes::Hash as BlockHash;

use crate::ChainCoordinate;

/// Minimal chain state tracking for the L1 bridge.
///
/// Tracks the last processed coordinate (for v2 API calls and index tracking),
/// and a hash→index map for finalization lookups.
pub struct ChainState {
    /// The last processed coordinate (hash + index).
    last_processed: Option<ChainCoordinate>,
    /// Mapping from block hash to index for finalization tracking.
    hash_to_index: HashMap<BlockHash, u64>,
    /// The last finalized coordinate.
    last_finalized: Option<ChainCoordinate>,
}

impl ChainState {
    /// Creates a new chain state.
    pub fn new(
        last_processed: Option<ChainCoordinate>,
        last_finalized: Option<ChainCoordinate>,
    ) -> Self {
        let mut hash_to_index = HashMap::new();

        if let Some(coord) = last_processed {
            hash_to_index.insert(coord.hash(), coord.index());
        }

        if let Some(coord) = last_finalized {
            hash_to_index.insert(coord.hash(), coord.index());
        }

        Self { last_processed, hash_to_index, last_finalized }
    }

    /// Returns the last processed coordinate.
    pub fn last_processed(&self) -> Option<ChainCoordinate> {
        self.last_processed
    }

    /// Allocates the next index and records the block.
    pub fn add_block(&mut self, hash: BlockHash) -> u64 {
        let index = self.last_processed.map(|c| c.index()).unwrap_or(0) + 1;
        self.last_processed = Some(ChainCoordinate::new(hash, index));
        self.hash_to_index.insert(hash, index);
        index
    }

    /// Rolls back by the given number of blocks, returning the new index.
    pub fn rollback(&mut self, num_blocks: u64) -> u64 {
        let current = self.last_processed.map(|c| c.index()).unwrap_or(0);
        let new_index = current.saturating_sub(num_blocks);
        // Prune entries after rollback point.
        self.hash_to_index.retain(|_, &mut i| i <= new_index);
        // Restore last_processed from hash_to_index.
        self.last_processed = self
            .hash_to_index
            .iter()
            .find(|(_, &i)| i == new_index)
            .map(|(h, _)| ChainCoordinate::new(*h, new_index));
        new_index
    }

    /// Looks up the index for a block hash.
    pub fn get_index(&self, hash: &BlockHash) -> Option<u64> {
        self.hash_to_index.get(hash).copied()
    }

    /// Returns the last finalized coordinate.
    pub fn last_finalized(&self) -> Option<ChainCoordinate> {
        self.last_finalized
    }

    /// Sets the last finalized coordinate and prunes old hash mappings.
    pub fn set_last_finalized(&mut self, coord: ChainCoordinate) {
        self.last_finalized = Some(coord);
        // Prune hash mappings for blocks before the finalized index.
        self.hash_to_index.retain(|_, &mut i| i >= coord.index());
    }
}
