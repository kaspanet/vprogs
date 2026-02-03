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
    /// Creates a sentinel root coordinate (ZERO_HASH, index 0, no links).
    pub fn root() -> Self {
        Self::new(ZERO_HASH, 0)
    }

    /// Creates a new standalone chain block coordinate (no links).
    pub fn new(hash: BlockHash, index: u64) -> Self {
        Self(std::sync::Arc::new(ChainBlockData {
            hash,
            index,
            prev: ArcSwapOption::empty(),
            next: ArcSwapOption::empty(),
        }))
    }

    /// Creates a new chain block coordinate linked to a previous one.
    pub(crate) fn new_linked(hash: BlockHash, index: u64, prev: ChainBlock) -> Self {
        Self(std::sync::Arc::new(ChainBlockData {
            hash,
            index,
            prev: ArcSwapOption::new(Some(prev.0)),
            next: ArcSwapOption::empty(),
        }))
    }
}

impl ChainBlockData {
    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    /// Returns the sequential index.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Returns the previous coordinate in the chain.
    pub fn prev(&self) -> Option<ChainBlock> {
        self.prev.load_full().map(ChainBlock)
    }

    /// Returns the next coordinate in the chain.
    pub fn next(&self) -> Option<ChainBlock> {
        self.next.load_full().map(ChainBlock)
    }

    /// Sets the next coordinate.
    pub(crate) fn set_next(&self, next: Option<ChainBlock>) {
        self.next.store(next.map(|c| c.0));
    }

    /// Clears the previous pointer.
    pub(crate) fn clear_prev(&self) {
        self.prev.store(None);
    }
}

impl std::fmt::Debug for ChainBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainBlock").field("hash", &self.hash).field("index", &self.index).finish()
    }
}
