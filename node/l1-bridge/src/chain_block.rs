use arc_swap::ArcSwapOption;
use kaspa_hashes::{Hash as BlockHash, ZERO_HASH};
use vprogs_core_macros::smart_pointer;

/// A position in the chain: block hash and sequential index.
/// Forms a doubly-linked list for efficient traversal.
#[smart_pointer]
pub struct ChainBlock {
    hash: BlockHash,
    index: u64,
    prev: ArcSwapOption<ChainBlockData>,
    next: ArcSwapOption<ChainBlockData>,
}

impl ChainBlock {
    /// Creates a new standalone chain block (no links).
    pub fn new(hash: BlockHash, index: u64) -> Self {
        Self(std::sync::Arc::new(ChainBlockData {
            hash,
            index,
            prev: ArcSwapOption::empty(),
            next: ArcSwapOption::empty(),
        }))
    }

    /// Creates and appends a new block after this one, returning it.
    pub(crate) fn append_next(&self, hash: BlockHash) -> Self {
        let next = Self(std::sync::Arc::new(ChainBlockData {
            hash,
            index: self.index + 1,
            prev: ArcSwapOption::new(Some(self.0.clone())),
            next: ArcSwapOption::empty(),
        }));
        self.next.store(Some(next.clone().0));
        next
    }

    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    /// Returns the sequential index.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Returns the previous block in the chain.
    pub fn prev(&self) -> Option<ChainBlock> {
        self.prev.load_full().map(ChainBlock)
    }

    /// Returns the next block in the chain.
    pub fn next(&self) -> Option<ChainBlock> {
        self.next.load_full().map(ChainBlock)
    }

    /// Clears the next pointer.
    pub(crate) fn clear_next(&self) {
        self.next.store(None);
    }

    /// Clears the previous pointer.
    pub(crate) fn clear_prev(&self) {
        self.prev.store(None);
    }
}

impl Default for ChainBlock {
    /// Creates a sentinel root block (ZERO_HASH, index 0, no links).
    fn default() -> Self {
        Self::new(ZERO_HASH, 0)
    }
}

impl std::fmt::Debug for ChainBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainBlock").field("hash", &self.hash).field("index", &self.index).finish()
    }
}
