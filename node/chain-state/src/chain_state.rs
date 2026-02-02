use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwapOption;
use crossbeam_queue::SegQueue;
use dashmap::DashMap;

use crate::{
    BlockHash, BlockState, ChainStateConfig, ChainStateCoordinate, ContentEntry, HeaderEntry,
    HeaderEntryData,
};

/// Manages the chain perception window with DAG support.
///
/// Tracks block headers from the pruning point to the tip, manages download
/// queues for headers and content, and provides efficient lookup and pruning.
pub struct ChainState {
    /// Header entries indexed by hash.
    entries: DashMap<BlockHash, HeaderEntry>,

    /// Blocks awaiting parent indices (separate from main entries for clarity).
    pending_index: DashMap<BlockHash, HeaderEntry>,

    /// Queue of hashes needing header download (present -> past direction).
    header_queue: SegQueue<BlockHash>,

    /// Queue of hashes needing content download (past -> present direction).
    content_queue: SegQueue<BlockHash>,

    /// Latest processed block pointer.
    latest: ArcSwapOption<HeaderEntryData>,

    /// Pruning point pointer.
    pruning_point: ArcSwapOption<HeaderEntryData>,

    /// Last finalized block pointer (for backward compatibility).
    last_finalized: ArcSwapOption<HeaderEntryData>,

    /// Configuration.
    config: ChainStateConfig,

    // === Backward compatibility fields (for l1-bridge) ===
    /// Initial index passed at construction.
    initial_index: AtomicU64,
    /// Current sequential index counter.
    current_index: AtomicU64,
}

impl ChainState {
    /// Creates a new ChainState with the given configuration.
    pub fn new(config: ChainStateConfig) -> Self {
        Self {
            entries: DashMap::new(),
            pending_index: DashMap::new(),
            header_queue: SegQueue::new(),
            content_queue: SegQueue::new(),
            latest: ArcSwapOption::empty(),
            pruning_point: ArcSwapOption::empty(),
            last_finalized: ArcSwapOption::empty(),
            config,
            initial_index: AtomicU64::new(0),
            current_index: AtomicU64::new(0),
        }
    }

    /// Creates a new ChainState for backward compatibility with l1-bridge.
    ///
    /// This constructor maintains the old API signature.
    /// Note: Indices start from 1 in the new design (0 = unassigned).
    pub fn new_legacy(
        last_processed: Option<ChainStateCoordinate>,
        last_finalized: Option<ChainStateCoordinate>,
    ) -> Self {
        let config = ChainStateConfig::default();
        let state = Self::new(config);

        let last_index = match last_processed {
            Some(coord) => {
                // Use the provided index, ensuring it's at least 1.
                let index = coord.index().max(1);
                let entry = HeaderEntry::new(coord.hash());
                let _ = entry.set_index(index);
                state.entries.insert(coord.hash(), entry.clone());
                state.latest.store(Some(entry.0));
                index
            }
            None => 0, // Will start from 1 on first next_index() call
        };

        if let Some(coord) = last_finalized {
            let index = coord.index().max(1);
            let entry = if let Some(e) = state.entries.get(&coord.hash()) {
                e.clone()
            } else {
                let e = HeaderEntry::new(coord.hash());
                let _ = e.set_index(index);
                state.entries.insert(coord.hash(), e.clone());
                e
            };
            state.last_finalized.store(Some(entry.0));
        }

        state.initial_index.store(last_index, Ordering::Release);
        state.current_index.store(last_index, Ordering::Release);
        state
    }

    /// Creates a ChainState from a checkpoint.
    ///
    /// The pruning point is assigned index 1, and latest is placed relative to it.
    pub fn from_checkpoint(
        pruning: ChainStateCoordinate,
        latest: Option<ChainStateCoordinate>,
        config: ChainStateConfig,
    ) -> Self {
        let state = Self::new(config);

        // Create and register pruning point entry.
        let pruning_entry = HeaderEntry::new(pruning.hash());
        let _ = pruning_entry.set_index(pruning.index().max(1)); // Ensure at least 1
        state.entries.insert(pruning.hash(), pruning_entry.clone());
        state.pruning_point.store(Some(pruning_entry.0.clone()));
        state.last_finalized.store(Some(pruning_entry.0));

        let mut max_index = pruning.index();

        // Create and register latest entry if different from pruning.
        if let Some(latest_coord) = latest {
            max_index = max_index.max(latest_coord.index());
            if latest_coord.hash() != pruning.hash() {
                let latest_entry = HeaderEntry::new(latest_coord.hash());
                let _ = latest_entry.set_index(latest_coord.index());
                state.entries.insert(latest_coord.hash(), latest_entry.clone());
                state.latest.store(Some(latest_entry.0));
            } else {
                state.latest.store(state.pruning_point.load_full());
            }
        }

        state.initial_index.store(max_index, Ordering::Release);
        state.current_index.store(max_index, Ordering::Release);
        state
    }

    // =========================================================================
    // Discovery and Reception
    // =========================================================================

    /// Discovers a new block hash, creating an entry and queueing for download.
    ///
    /// Returns the entry (existing or newly created).
    pub fn discover(&self, hash: BlockHash) -> HeaderEntry {
        // Try to get existing entry.
        if let Some(entry) = self.entries.get(&hash) {
            return entry.clone();
        }

        // Create new entry and try to insert.
        let entry = HeaderEntry::new(hash);
        match self.entries.entry(hash) {
            dashmap::mapref::entry::Entry::Occupied(e) => e.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let entry = entry.clone();
                e.insert(entry.clone());
                // Queue for header download.
                self.header_queue.push(hash);
                entry
            }
        }
    }

    /// Receives header data for a block.
    ///
    /// This triggers parent discovery and index propagation.
    pub fn receive_header(&self, hash: BlockHash, parents: Vec<BlockHash>, daa_score: u64) {
        let entry = self.discover(hash);

        // Store header data.
        if !entry.receive_header(parents.clone(), daa_score) {
            return; // Already received.
        }

        // Process parents and try to assign index.
        self.process_parents(&entry, &parents);
    }

    /// Processes parent relationships for an entry.
    fn process_parents(&self, entry: &HeaderEntry, parents: &[BlockHash]) {
        if parents.is_empty() {
            // Genesis or pruning point - assign index 1 if not already set.
            if let Some(children) = entry.set_index(1) {
                self.content_queue.push(entry.hash());
                // Propagate to children.
                for child_hash in children {
                    self.try_assign_index(child_hash);
                }
            }
            return;
        }

        let mut max_parent_index = 0u64;
        let mut all_parents_have_index = true;

        for &parent_hash in parents {
            let parent = self.discover(parent_hash);

            let parent_index = parent.index();
            if parent_index == 0 {
                // Parent doesn't have index yet, register as pending child.
                // If parent got index between discover and add_pending_child,
                // we'll catch it below when we re-check all_parents_have_index.
                let _ = parent.add_pending_child(entry.downgrade());
                self.pending_index.insert(entry.hash(), entry.clone());
                all_parents_have_index = false;
            } else {
                max_parent_index = max_parent_index.max(parent_index);
            }
        }

        if all_parents_have_index {
            let new_index = max_parent_index + 1;
            if let Some(children) = entry.set_index(new_index) {
                self.pending_index.remove(&entry.hash());
                self.content_queue.push(entry.hash());
                // Propagate to children.
                for child_hash in children {
                    self.try_assign_index(child_hash);
                }
            }
        }
    }

    /// Called when a parent's index becomes known to re-check pending children.
    fn try_assign_index(&self, hash: BlockHash) {
        let entry = {
            let Some(entry_ref) = self.pending_index.get(&hash) else {
                return;
            };
            entry_ref.clone()
        }; // Ref dropped here

        let Some(parents) = entry.parents() else {
            return;
        };

        let mut max_parent_index = 0u64;
        let mut all_parents_have_index = true;

        for parent_hash in parents {
            if let Some(parent) = self.entries.get(parent_hash) {
                let parent_index = parent.index();
                if parent_index == 0 {
                    all_parents_have_index = false;
                    break;
                }
                max_parent_index = max_parent_index.max(parent_index);
            } else {
                all_parents_have_index = false;
                break;
            }
        }

        if all_parents_have_index {
            let new_index = max_parent_index + 1;
            if let Some(children) = entry.set_index(new_index) {
                self.pending_index.remove(&hash);
                self.content_queue.push(hash);
                // Propagate to children (recursive).
                for child_hash in children {
                    self.try_assign_index(child_hash);
                }
            }
        }
    }

    /// Receives block content.
    pub fn receive_content(&self, hash: BlockHash, data: Vec<u8>) {
        let Some(entry) = self.entries.get(&hash) else {
            return;
        };

        let index = entry.index();
        if index == 0 {
            return; // Index not assigned yet.
        }

        let content = Arc::new(ContentEntry { index, data });
        entry.receive_content(content);
    }

    // =========================================================================
    // Download Queue APIs
    // =========================================================================

    /// Returns the next hash needing header download.
    ///
    /// Downloads propagate from present (tip) toward past (pruning point).
    pub fn next_header_to_download(&self) -> Option<BlockHash> {
        while let Some(hash) = self.header_queue.pop() {
            // Verify still needs downloading.
            if let Some(entry) = self.entries.get(&hash) {
                if entry.state() == BlockState::HeaderPending {
                    return Some(hash);
                }
            }
        }
        None
    }

    /// Returns up to `limit` hashes needing header download.
    pub fn headers_to_download(&self, limit: usize) -> Vec<BlockHash> {
        let mut result = Vec::with_capacity(limit);
        while result.len() < limit {
            match self.next_header_to_download() {
                Some(hash) => result.push(hash),
                None => break,
            }
        }
        result
    }

    /// Returns the next hash needing content download.
    ///
    /// Downloads propagate from past (pruning point) toward present (tip).
    pub fn next_content_to_download(&self) -> Option<BlockHash> {
        while let Some(hash) = self.content_queue.pop() {
            // Verify still needs downloading.
            if let Some(entry) = self.entries.get(&hash) {
                if entry.state() == BlockState::ContentPending {
                    return Some(hash);
                }
            }
        }
        None
    }

    /// Returns up to `limit` hashes needing content download.
    pub fn content_to_download(&self, limit: usize) -> Vec<BlockHash> {
        let mut result = Vec::with_capacity(limit);
        while result.len() < limit {
            match self.next_content_to_download() {
                Some(hash) => result.push(hash),
                None => break,
            }
        }
        result
    }

    // =========================================================================
    // Query APIs
    // =========================================================================

    /// Gets the entry for a hash.
    pub fn get_entry(&self, hash: &BlockHash) -> Option<HeaderEntry> {
        self.entries.get(hash).map(|e| e.clone())
    }

    /// Gets the coordinate for a hash.
    pub fn get_coordinate(&self, hash: &BlockHash) -> Option<ChainStateCoordinate> {
        let entry = self.entries.get(hash)?;
        let index = entry.index();
        if index == 0 { None } else { Some(ChainStateCoordinate::new(*hash, index)) }
    }

    /// Gets content for a hash.
    pub fn get_content(&self, hash: &BlockHash) -> Option<Arc<ContentEntry>> {
        let entry = self.entries.get(hash)?;
        entry.content()
    }

    /// Gets content by index (returns first match if multiple blocks share the index).
    pub fn get_content_by_index(&self, index: u64) -> Option<Arc<ContentEntry>> {
        for entry in self.entries.iter() {
            if entry.index() == index {
                if let Some(content) = entry.content() {
                    return Some(content);
                }
            }
        }
        None
    }

    /// Gets all hashes at a given index.
    pub fn get_hashes_at_index(&self, index: u64) -> Vec<BlockHash> {
        self.entries.iter().filter(|e| e.index() == index).map(|e| e.hash()).collect()
    }

    /// Returns the number of tracked entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if no entries are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // =========================================================================
    // Pointer Management
    // =========================================================================

    /// Gets the latest block entry.
    pub fn latest(&self) -> Option<HeaderEntry> {
        self.latest.load_full().map(|data| HeaderEntry(data))
    }

    /// Sets the latest block by hash.
    pub fn set_latest(&self, hash: BlockHash) {
        if let Some(entry) = self.entries.get(&hash) {
            self.latest.store(Some(entry.0.clone()));
        }
    }

    /// Sets the latest block entry directly.
    pub fn set_latest_entry(&self, entry: HeaderEntry) {
        self.latest.store(Some(entry.0));
    }

    /// Gets the pruning point entry.
    pub fn pruning_point(&self) -> Option<HeaderEntry> {
        self.pruning_point.load_full().map(|data| HeaderEntry(data))
    }

    /// Sets the pruning point and triggers pruning.
    pub fn set_pruning_point(&self, hash: BlockHash) {
        if let Some(entry) = self.entries.get(&hash) {
            self.pruning_point.store(Some(entry.0.clone()));
            self.prune_to(entry.index());
        }
    }

    // =========================================================================
    // Pruning
    // =========================================================================

    /// Prunes all entries with index < pruning_index.
    fn prune_to(&self, pruning_index: u64) {
        // Remove entries below pruning point.
        self.entries.retain(|_, entry| entry.index() >= pruning_index || entry.index() == 0);

        // Remove pending entries below pruning point.
        self.pending_index.retain(|_, entry| entry.index() >= pruning_index || entry.index() == 0);
    }

    /// Returns the configuration.
    pub fn config(&self) -> &ChainStateConfig {
        &self.config
    }

    // =========================================================================
    // Backward Compatibility (for l1-bridge)
    // =========================================================================

    /// Returns the initial index (backward compatibility).
    pub fn initial_index(&self) -> u64 {
        self.initial_index.load(Ordering::Acquire)
    }

    /// Returns the current index counter (backward compatibility).
    pub fn current_index(&self) -> u64 {
        self.current_index.load(Ordering::Acquire)
    }

    /// Increments and returns the next index (backward compatibility).
    pub fn next_index(&mut self) -> u64 {
        self.current_index.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Records a block coordinate (backward compatibility).
    /// Note: Index 0 is treated as 1 since 0 means "unassigned" in the new design.
    pub fn record_block(&self, coord: ChainStateCoordinate) {
        let index = coord.index().max(1);
        let entry = self.discover(coord.hash());
        let _ = entry.set_index(index);
    }

    /// Returns the last processed coordinate (backward compatibility).
    pub fn last_processed(&self) -> Option<ChainStateCoordinate> {
        self.latest().map(|e| ChainStateCoordinate::new(e.hash(), e.index()))
    }

    /// Sets the last processed coordinate (backward compatibility).
    /// Note: Index 0 is treated as 1 since 0 means "unassigned" in the new design.
    pub fn set_last_processed(&self, coord: ChainStateCoordinate) {
        let index = coord.index().max(1);
        self.record_block(ChainStateCoordinate::new(coord.hash(), index));
        self.set_latest(coord.hash());
        self.current_index.store(index, Ordering::Release);
    }

    /// Returns the last finalized coordinate (backward compatibility).
    pub fn last_finalized(&self) -> Option<ChainStateCoordinate> {
        self.last_finalized.load_full().map(|data| {
            let entry = HeaderEntry(data);
            ChainStateCoordinate::new(entry.hash(), entry.index())
        })
    }

    /// Sets the last finalized coordinate (backward compatibility).
    /// Note: Index 0 is treated as 1 since 0 means "unassigned" in the new design.
    pub fn set_last_finalized(&self, coord: ChainStateCoordinate) {
        let index = coord.index().max(1);
        let entry = self.discover(coord.hash());
        let _ = entry.set_index(index);
        self.last_finalized.store(Some(entry.0));
        // Prune hash mappings for blocks before the finalized index.
        self.prune_to(index);
    }

    /// Looks up the coordinate for a block hash (backward compatibility alias).
    pub fn get_coordinate_for_hash(&self, hash: &BlockHash) -> Option<ChainStateCoordinate> {
        self.get_coordinate(hash)
    }
}

impl Default for ChainState {
    fn default() -> Self {
        Self::new(ChainStateConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use kaspa_hashes::Hash;

    use super::*;

    fn hash(n: u8) -> BlockHash {
        Hash::from_bytes([n; 32])
    }

    #[test]
    fn test_discover_creates_entry() {
        let state = ChainState::default();
        let h = hash(1);

        let entry = state.discover(h);
        assert_eq!(entry.hash(), h);
        assert_eq!(entry.state(), BlockState::HeaderPending);
        assert_eq!(entry.index(), 0);

        // Discover again returns same entry.
        let entry2 = state.discover(h);
        assert_eq!(entry.hash(), entry2.hash());
    }

    #[test]
    fn test_receive_header_no_parents() {
        let state = ChainState::default();
        let h = hash(1);

        state.receive_header(h, vec![], 100);

        let entry = state.get_entry(&h).unwrap();
        assert_eq!(entry.state(), BlockState::ContentPending);
        assert_eq!(entry.index(), 1);
        assert_eq!(entry.daa_score(), 100);
    }

    #[test]
    fn test_receive_header_with_known_parent() {
        let state = ChainState::default();
        let parent = hash(1);
        let child = hash(2);

        // Add parent with index.
        state.receive_header(parent, vec![], 100);
        assert_eq!(state.get_entry(&parent).unwrap().index(), 1);

        // Add child referencing parent.
        state.receive_header(child, vec![parent], 200);
        assert_eq!(state.get_entry(&child).unwrap().index(), 2);
    }

    #[test]
    fn test_receive_header_with_unknown_parent() {
        let state = ChainState::default();
        let parent = hash(1);
        let child = hash(2);

        // Add child first (parent unknown).
        state.receive_header(child, vec![parent], 200);

        // Child should be pending.
        let child_entry = state.get_entry(&child).unwrap();
        assert_eq!(child_entry.state(), BlockState::HeaderKnown);
        assert_eq!(child_entry.index(), 0);
        drop(child_entry);

        // Parent should be queued for download.
        assert_eq!(state.next_header_to_download(), Some(parent));

        // Now add parent - this should automatically propagate index to child.
        state.receive_header(parent, vec![], 100);

        // Child should now have index (auto-propagated).
        assert_eq!(state.get_entry(&child).unwrap().index(), 2);
    }

    #[test]
    fn test_dag_multiple_parents() {
        let state = ChainState::default();
        let genesis = hash(0);
        let a = hash(1);
        let b = hash(2);
        let merge = hash(3);

        // Genesis.
        state.receive_header(genesis, vec![], 100);
        assert_eq!(state.get_entry(&genesis).unwrap().index(), 1);

        // Two children of genesis.
        state.receive_header(a, vec![genesis], 200);
        state.receive_header(b, vec![genesis], 200);
        assert_eq!(state.get_entry(&a).unwrap().index(), 2);
        assert_eq!(state.get_entry(&b).unwrap().index(), 2);

        // Merge block with both as parents.
        state.receive_header(merge, vec![a, b], 300);
        assert_eq!(state.get_entry(&merge).unwrap().index(), 3); // max(2, 2) + 1
    }

    #[test]
    fn test_get_hashes_at_index() {
        let state = ChainState::default();
        let genesis = hash(0);
        let a = hash(1);
        let b = hash(2);

        state.receive_header(genesis, vec![], 100);
        state.receive_header(a, vec![genesis], 200);
        state.receive_header(b, vec![genesis], 200);

        let at_index_2 = state.get_hashes_at_index(2);
        assert_eq!(at_index_2.len(), 2);
        assert!(at_index_2.contains(&a));
        assert!(at_index_2.contains(&b));
    }

    #[test]
    fn test_pruning() {
        let state = ChainState::default();

        // Build a chain: hash(0) has no parents (index 1), hash(i) has parent hash(i-1).
        // So hash(i) gets index i+1.
        for i in 0..10u8 {
            let h = hash(i);
            let parents = if i == 0 { vec![] } else { vec![hash(i - 1)] };
            state.receive_header(h, parents, i as u64 * 100);
        }

        // Verify indices: hash(i) should have index i+1.
        for i in 0..10u8 {
            assert_eq!(state.get_entry(&hash(i)).unwrap().index(), (i + 1) as u64);
        }

        // All should be present.
        assert_eq!(state.len(), 10);

        // Prune to index 5. This keeps entries with index >= 5.
        // hash(0) has index 1, hash(1) has index 2, ..., hash(4) has index 5.
        state.prune_to(5);

        // Entries with index < 5 should be gone (hash(0) through hash(3), indices 1-4).
        for i in 0..4u8 {
            assert!(
                state.get_entry(&hash(i)).is_none(),
                "hash({}) with index {} should be pruned",
                i,
                i + 1
            );
        }

        // Entries with index >= 5 should remain (hash(4) through hash(9), indices 5-10).
        for i in 4..10u8 {
            assert!(
                state.get_entry(&hash(i)).is_some(),
                "hash({}) with index {} should remain",
                i,
                i + 1
            );
        }
    }

    #[test]
    fn test_content_reception() {
        let state = ChainState::default();
        let h = hash(1);

        state.receive_header(h, vec![], 100);
        state.receive_content(h, vec![1, 2, 3]);

        let entry = state.get_entry(&h).unwrap();
        assert_eq!(entry.state(), BlockState::ContentKnown);

        let content = state.get_content(&h).unwrap();
        assert_eq!(content.data, vec![1, 2, 3]);
    }

    #[test]
    fn test_from_checkpoint() {
        let pruning = ChainStateCoordinate::new(hash(0), 100);
        let latest = ChainStateCoordinate::new(hash(1), 150);

        let state = ChainState::from_checkpoint(pruning, Some(latest), ChainStateConfig::default());

        assert_eq!(state.pruning_point().unwrap().index(), 100);
        assert_eq!(state.latest().unwrap().index(), 150);
    }
}
