use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use kaspa_hashes::Hash as BlockHash;
use parking_lot::RwLock;

use crate::ChainCoordinate;

/// Tracks the state of the L1 bridge.
pub struct BridgeState {
    connected: AtomicBool,
    synced: AtomicBool,
    /// The last processed block.
    last_processed_block: RwLock<Option<ChainCoordinate>>,
    /// The initial index passed at construction (reference point for index calculations).
    initial_index: u64,
    /// Current block index (incremented for each block added).
    current_index: AtomicU64,
    /// Mapping from block hash to index for finalization tracking.
    hash_to_index: RwLock<HashMap<BlockHash, u64>>,
    /// The last finalized block we've reported.
    last_finalized_block: RwLock<Option<ChainCoordinate>>,
    reconnect_count: AtomicU64,
}

impl BridgeState {
    /// Creates a new bridge state tracker.
    ///
    /// - `last_processed_block`: The last processed block, or None to start fresh
    /// - `last_finalized_block`: The last finalized block, or None if unknown
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
            connected: AtomicBool::new(false),
            synced: AtomicBool::new(false),
            last_processed_block: RwLock::new(last_processed_block),
            initial_index: last_index,
            current_index: AtomicU64::new(last_index),
            hash_to_index: RwLock::new(hash_to_index),
            last_finalized_block: RwLock::new(last_finalized_block),
            reconnect_count: AtomicU64::new(0),
        }
    }

    /// Returns the initial index (the starting point for index calculations).
    pub fn initial_index(&self) -> u64 {
        self.initial_index
    }

    /// Records a block coordinate for finalization tracking.
    pub fn record_block(&self, block: ChainCoordinate) {
        self.hash_to_index.write().insert(block.hash(), block.index());
    }

    /// Looks up the coordinate for a block hash (if tracked).
    pub fn get_coordinate_for_hash(&self, hash: &BlockHash) -> Option<ChainCoordinate> {
        self.hash_to_index.read().get(hash).map(|&index| ChainCoordinate::new(*hash, index))
    }

    /// Returns the last finalized coordinate.
    pub fn last_finalized(&self) -> Option<ChainCoordinate> {
        *self.last_finalized_block.read()
    }

    /// Updates the last finalized coordinate and prunes old hash mappings.
    pub fn set_last_finalized(&self, coord: ChainCoordinate) {
        *self.last_finalized_block.write() = Some(coord);
        // Prune hash mappings for blocks before the finalized index.
        self.hash_to_index.write().retain(|_, &mut i| i >= coord.index());
    }

    /// Returns whether the bridge is currently connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Sets the connection state.
    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Release);
    }

    /// Returns whether the bridge has completed initial sync.
    pub fn is_synced(&self) -> bool {
        self.synced.load(Ordering::Acquire)
    }

    /// Sets the synced state.
    pub fn set_synced(&self, synced: bool) {
        self.synced.store(synced, Ordering::Release);
    }

    /// Returns the last processed coordinate.
    pub fn last_processed(&self) -> Option<ChainCoordinate> {
        *self.last_processed_block.read()
    }

    /// Updates the last processed coordinate.
    pub fn set_last_processed(&self, coord: ChainCoordinate) {
        *self.last_processed_block.write() = Some(coord);
        self.current_index.store(coord.index(), Ordering::Release);
    }

    /// Returns the current block index.
    pub fn current_index(&self) -> u64 {
        self.current_index.load(Ordering::Acquire)
    }

    /// Increments the index and returns the new value.
    pub fn next_index(&self) -> u64 {
        self.current_index.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Increments the reconnect count and returns the new value.
    pub(crate) fn increment_reconnect_count(&self) -> u64 {
        self.reconnect_count.fetch_add(1, Ordering::Relaxed) + 1
    }
}
