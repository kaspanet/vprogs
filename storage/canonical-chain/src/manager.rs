use std::{
    collections::{HashMap, VecDeque},
    iter::empty,
};

use vprogs_core_types::BatchMetadata;

use crate::{append_outcome::AppendOutcome, chain::CanonicalChain};

/// Single-writer handle for the canonical chain, backed by an append-only batch-metadata log.
pub struct CanonicalChainManager<M> {
    /// The canonical-bit oracle this manager drives and shares with readers.
    chain: CanonicalChain,
    /// Lowest live id; ids below it are finalized and their entries dropped.
    base: u64,
    /// Metadata of each live id in order: `entries[i]` is the metadata of id `base + i`.
    entries: VecDeque<M>,
    /// Reverse lookup from block hash to its id.
    index: HashMap<[u8; 32], u64>,
}

impl<M: BatchMetadata> CanonicalChainManager<M> {
    /// Creates a manager over `chain`, replaying persisted `(id, metadata)` entries in order.
    pub fn new(chain: CanonicalChain, entries: impl IntoIterator<Item = (u64, M)>) -> Self {
        // Claim the sole-writer role and start with an empty log.
        chain.claim_writer();
        let mut manager = Self { chain, base: 1, entries: VecDeque::new(), index: HashMap::new() };

        // Replay each persisted batch into the log; the first id sets the base, the last the tip.
        let mut tip = None;
        for (id, metadata) in entries {
            if manager.entries.is_empty() {
                manager.base = id;
            }
            let assigned = manager.push(metadata);
            debug_assert_eq!(assigned, id, "restore must be contiguous");
            tip = Some(id);
        }
        let Some(tip) = tip else { return manager };

        // Project the tip's canonical ancestry onto the oracle in a single publish.
        let canonical = manager.canonical_ancestry(tip);
        manager.chain.restore(manager.base, tip, canonical);
        manager
    }

    /// Ingests `metadata` as a canonical block, returning its [`AppendOutcome`].
    pub fn append(&mut self, metadata: M) -> AppendOutcome {
        // Returning block: re-canonicalize its existing id.
        if let Some(id) = self.id(&metadata.block_hash()) {
            if id > self.chain.tip() {
                self.chain.append(id);
            }
            return AppendOutcome { id, is_new: false };
        }

        // New block: allocate a fresh id and canonicalize it.
        let id = self.push(metadata);
        self.chain.append(id);
        AppendOutcome { id, is_new: true }
    }

    /// Rolls the canonical chain back to `new_tip`, orphaning every id above it.
    pub fn rollback(&mut self, new_tip: u64) {
        self.chain.rollback(new_tip);
    }

    /// Finalizes ids below `below`: the chain reads them as canonical and their log entries drop.
    pub fn finalize(&mut self, below: u64) {
        // Finalize the canonical bits below `below`.
        self.chain.finalize(below);

        // Drop the now-finalized log entries and their reverse-index keys.
        while self.base < below {
            let Some(metadata) = self.entries.pop_front() else { break };
            self.index.remove(&metadata.block_hash());
            self.base += 1;
        }
    }

    /// Returns the read oracle to snapshot from.
    pub fn chain(&self) -> &CanonicalChain {
        &self.chain
    }

    /// Returns the id assigned to `block_hash`, if it has been seen.
    pub fn id(&self, block_hash: &[u8; 32]) -> Option<u64> {
        self.index.get(block_hash).copied()
    }

    /// Returns the metadata stored for `id`, or `None` if it is finalized or never assigned.
    pub fn metadata(&self, id: u64) -> Option<&M> {
        self.entries.get(id.checked_sub(self.base)? as usize)
    }

    /// Appends `metadata` at the next dense id and returns it (the log's allocate primitive).
    fn push(&mut self, metadata: M) -> u64 {
        let id = self.base + self.entries.len() as u64;
        self.index.insert(metadata.block_hash(), id);
        self.entries.push_back(metadata);
        id
    }

    /// Walks `tip`'s ancestry via `parent_id` to the finalized floor, collecting the canonical ids.
    fn canonical_ancestry(&self, tip: u64) -> Vec<u64> {
        let mut canonical = Vec::new();
        let mut id = tip;
        loop {
            canonical.push(id);
            let parent = self.metadata(id).expect("walked id is live").parent_id();
            if parent < self.base || parent >= id {
                break;
            }
            id = parent;
        }
        canonical
    }
}

impl<M: BatchMetadata> Default for CanonicalChainManager<M> {
    /// Creates a fresh, empty manager with its own new chain.
    fn default() -> Self {
        Self::new(CanonicalChain::default(), empty())
    }
}

impl<M> Drop for CanonicalChainManager<M> {
    /// Releases the chain's sole-writer role.
    fn drop(&mut self) {
        self.chain.release_writer();
    }
}
