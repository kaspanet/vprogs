use std::collections::HashMap;

use kaspa_hashes::Hash as BlockHash;

use crate::ChainCoordinate;

/// Tracks the chain synchronization state.
/// This is only accessed from the single-threaded worker runtime.
pub struct ChainState {
    /// The last processed block.
    last_processed_block: Option<ChainCoordinate>,
    /// The initial index passed at construction (reference point for index calculations).
    initial_index: u64,
    /// Current block index (incremented for each block added).
    current_index: u64,
    /// Mapping from block hash to index for finalization tracking.
    hash_to_index: HashMap<BlockHash, u64>,
    /// The last finalized block we've reported.
    last_finalized_block: Option<ChainCoordinate>,
}

impl ChainState {
    /// Creates a new chain state tracker.
    pub fn new(
        last_processed_block: Option<ChainCoordinate>,
        last_finalized_block: Option<ChainCoordinate>,
    ) -> Self {
        let mut hash_to_index = HashMap::new();

        let last_index = match last_processed_block {
            Some(coord) => {
                hash_to_index.insert(coord.hash(), coord.index());
                coord.index()
            }
            None => 0,
        };

        if let Some(coord) = last_finalized_block {
            hash_to_index.insert(coord.hash(), coord.index());
        }

        Self {
            last_processed_block,
            initial_index: last_index,
            current_index: last_index,
            hash_to_index,
            last_finalized_block,
        }
    }

    /// Returns the initial index (the starting point for index calculations).
    pub fn initial_index(&self) -> u64 {
        self.initial_index
    }

    /// Records a block coordinate for finalization tracking.
    pub fn record_block(&mut self, block: ChainCoordinate) {
        self.hash_to_index.insert(block.hash(), block.index());
    }

    /// Looks up the coordinate for a block hash (if tracked).
    pub fn get_coordinate_for_hash(&self, hash: &BlockHash) -> Option<ChainCoordinate> {
        self.hash_to_index.get(hash).map(|&index| ChainCoordinate::new(*hash, index))
    }

    /// Returns the last finalized coordinate.
    pub fn last_finalized(&self) -> Option<ChainCoordinate> {
        self.last_finalized_block
    }

    /// Updates the last finalized coordinate and prunes old hash mappings.
    pub fn set_last_finalized(&mut self, coord: ChainCoordinate) {
        self.last_finalized_block = Some(coord);
        // Prune hash mappings for blocks before the finalized index.
        self.hash_to_index.retain(|_, &mut i| i >= coord.index());
    }

    /// Returns the last processed coordinate.
    pub fn last_processed(&self) -> Option<ChainCoordinate> {
        self.last_processed_block
    }

    /// Updates the last processed coordinate.
    pub fn set_last_processed(&mut self, coord: ChainCoordinate) {
        self.last_processed_block = Some(coord);
        self.current_index = coord.index();
    }

    /// Returns the current block index.
    pub fn current_index(&self) -> u64 {
        self.current_index
    }

    /// Increments the index and returns the new value.
    pub fn next_index(&mut self) -> u64 {
        self.current_index += 1;
        self.current_index
    }
}
