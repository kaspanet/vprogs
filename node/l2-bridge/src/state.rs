use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use kaspa_hashes::Hash as BlockHash;
use parking_lot::RwLock;

/// Tracks the state of the L2 bridge.
pub struct BridgeState {
    connected: AtomicBool,
    synced: AtomicBool,
    last_block_hash: RwLock<Option<BlockHash>>,
    /// The initial index passed at construction (reference point for index calculations).
    initial_index: u64,
    /// Current block index (incremented for each block added).
    current_index: AtomicU64,
    /// Mapping from block hash to index for finalization tracking.
    hash_to_index: RwLock<HashMap<BlockHash, u64>>,
    /// The last finalized index we've reported.
    last_finalized_index: AtomicU64,
    reconnect_count: AtomicU64,
}

impl BridgeState {
    /// Creates a new bridge state tracker.
    ///
    /// - `last_block_hash`: The hash of the last processed block (or None to start from pruning
    ///   point)
    /// - `last_index`: The index of the last processed block (next block will be last_index + 1)
    pub fn new(last_block_hash: Option<BlockHash>, last_index: u64) -> Self {
        let mut hash_to_index = HashMap::new();
        // Record the starting point if provided.
        if let Some(hash) = last_block_hash {
            hash_to_index.insert(hash, last_index);
        }
        Self {
            connected: AtomicBool::new(false),
            synced: AtomicBool::new(false),
            last_block_hash: RwLock::new(last_block_hash),
            initial_index: last_index,
            current_index: AtomicU64::new(last_index),
            hash_to_index: RwLock::new(hash_to_index),
            last_finalized_index: AtomicU64::new(last_index),
            reconnect_count: AtomicU64::new(0),
        }
    }

    /// Returns the initial index (the starting point for index calculations).
    pub fn initial_index(&self) -> u64 {
        self.initial_index
    }

    /// Records a block hash and its index for finalization tracking.
    pub fn record_block(&self, hash: BlockHash, index: u64) {
        self.hash_to_index.write().insert(hash, index);
    }

    /// Looks up the index for a block hash.
    pub fn get_index_for_hash(&self, hash: &BlockHash) -> Option<u64> {
        self.hash_to_index.read().get(hash).copied()
    }

    /// Returns the last finalized index.
    pub fn last_finalized_index(&self) -> u64 {
        self.last_finalized_index.load(Ordering::Acquire)
    }

    /// Updates the last finalized index and prunes old hash mappings.
    pub fn set_finalized(&self, index: u64) {
        self.last_finalized_index.store(index, Ordering::Release);
        // Prune hash mappings for blocks before the finalized index.
        self.hash_to_index.write().retain(|_, &mut i| i >= index);
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

    /// Returns the last processed block hash.
    pub fn last_block_hash(&self) -> Option<BlockHash> {
        *self.last_block_hash.read()
    }

    /// Updates the last processed block hash.
    pub fn set_last_block_hash(&self, hash: BlockHash) {
        *self.last_block_hash.write() = Some(hash);
    }

    /// Returns the current block index.
    pub fn current_index(&self) -> u64 {
        self.current_index.load(Ordering::Acquire)
    }

    /// Increments the index and returns the new value.
    pub fn next_index(&self) -> u64 {
        self.current_index.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Sets the current index (used for rollback).
    pub fn set_index(&self, index: u64) {
        self.current_index.store(index, Ordering::Release);
    }

    /// Increments the reconnect count and returns the new value.
    pub(crate) fn increment_reconnect_count(&self) -> u64 {
        self.reconnect_count.fetch_add(1, Ordering::Relaxed) + 1
    }
}
