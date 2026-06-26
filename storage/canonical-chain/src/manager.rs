use std::{
    collections::{HashMap, VecDeque},
    iter::empty,
};

use vprogs_core_types::BatchMetadata;

use crate::chain::CanonicalChain;

/// Outcome of [`CanonicalChainManager::append`]: the block's id and whether it was freshly
/// allocated.
///
/// `is_new` is `false` when an existing id was reused (a block returning after a reorg); its
/// retained state can then be revived rather than re-executed.
#[derive(Debug, Clone, Copy)]
pub struct Appended {
    /// Canonical id of the appended block.
    pub id: u64,
    /// `true` if a fresh id was allocated; `false` if a returning block reused its existing id.
    pub is_new: bool,
}

/// Single-owner management handle for the canonical chain, held by the L1 bridge.
///
/// It is the append-only batch-metadata log: ids are dense from `base`, with a `block_hash -> id`
/// reverse index, so a finalized prefix drops cleanly. It also drives the canonical bits. `&mut
/// self` writes enforce the single-writer contract; [`chain`](Self::chain) hands out the lock-free
/// read oracle for everyone else.
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
        chain.claim_writer();
        let mut manager = Self { chain, base: 1, entries: VecDeque::new(), index: HashMap::new() };

        // 1. Replay every persisted batch into the log; the first id establishes the base.
        for (id, metadata) in entries {
            if manager.entries.is_empty() {
                manager.base = id;
            }
            let assigned = manager.push(metadata);
            debug_assert_eq!(assigned, id, "restore must be contiguous");
        }
        let Some(tip) = manager.last_id() else { return manager };

        // 2. The canonical chain is the tip's ancestry: walk parent_id to the finalized floor. The
        // highest id is always canonical (a reorg appends its heavier branch above the orphans).
        let mut canonical = Vec::new();
        let mut id = tip;
        loop {
            canonical.push(id);
            let parent = manager.metadata(id).expect("walked id is live").parent_id();
            if parent < manager.base || parent >= id {
                break;
            }
            id = parent;
        }

        // 3. Project the canonical set onto the oracle in a single publish.
        manager.chain.restore(manager.base, tip, canonical);
        manager
    }

    /// Ingests `metadata` as a canonical block, returning its [`Appended`] outcome.
    pub fn append(&mut self, metadata: M) -> Appended {
        if let Some(id) = self.id(&metadata.block_hash()) {
            // Returning block: re-canonicalize it.
            if id > self.chain.tip() {
                self.chain.append(id);
            }
            return Appended { id, is_new: false };
        }
        let id = self.push(metadata);
        self.chain.append(id);
        Appended { id, is_new: true }
    }

    /// Rolls the canonical chain back to `new_tip`, orphaning every id above it.
    pub fn rollback(&mut self, new_tip: u64) {
        self.chain.rollback(new_tip);
    }

    /// Finalizes ids below `below`: the chain reads them as canonical and their log entries drop.
    pub fn finalize(&mut self, below: u64) {
        self.chain.finalize(below);
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

    /// The highest live id, or `None` if the log is empty.
    fn last_id(&self) -> Option<u64> {
        (!self.entries.is_empty()).then(|| self.base + self.entries.len() as u64 - 1)
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
