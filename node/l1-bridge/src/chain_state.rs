use std::collections::HashMap;

use kaspa_hashes::Hash as BlockHash;

use crate::ChainCoordinate;

/// Minimal chain state tracking for the L1 bridge.
///
/// Tracks the last processed block hash (for v2 API calls), current index counter,
/// and a hash→index map for finalization lookups.
pub struct ChainState {
    /// The last processed block hash.
    last_processed_hash: Option<BlockHash>,
    /// Current block index (incremented for each block added).
    current_index: u64,
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

        let (last_hash, last_index) = match last_processed {
            Some(coord) => {
                hash_to_index.insert(coord.hash(), coord.index());
                (Some(coord.hash()), coord.index())
            }
            None => (None, 0),
        };

        if let Some(coord) = last_finalized {
            hash_to_index.insert(coord.hash(), coord.index());
        }

        Self {
            last_processed_hash: last_hash,
            current_index: last_index,
            hash_to_index,
            last_finalized,
        }
    }

    /// Returns the last processed block hash (used as start_hash for v2 API).
    pub fn last_processed_hash(&self) -> Option<BlockHash> {
        self.last_processed_hash
    }

    /// Returns the current block index.
    pub fn current_index(&self) -> u64 {
        self.current_index
    }

    /// Allocates the next index and records the block.
    pub fn add_block(&mut self, hash: BlockHash) -> u64 {
        self.current_index += 1;
        self.last_processed_hash = Some(hash);
        self.hash_to_index.insert(hash, self.current_index);
        self.current_index
    }

    /// Rolls back to a given index and hash.
    pub fn rollback_to(&mut self, hash: BlockHash, index: u64) {
        self.current_index = index;
        self.last_processed_hash = Some(hash);
        // Prune entries after rollback point.
        self.hash_to_index.retain(|_, &mut i| i <= index);
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
