use arc_swap::ArcSwapOption;
use kaspa_hashes::Hash as BlockHash;
use vprogs_core_macros::smart_pointer;

/// A position in the chain: block hash and sequential index.
/// Forms a doubly-linked list for efficient traversal.
#[smart_pointer]
pub struct ChainCoordinate {
    hash: BlockHash,
    index: u64,
    prev: ArcSwapOption<ChainCoordinateData>,
    next: ArcSwapOption<ChainCoordinateData>,
}

impl ChainCoordinate {
    /// Creates a new standalone chain coordinate (no links).
    pub fn new(hash: BlockHash, index: u64) -> Self {
        Self(std::sync::Arc::new(ChainCoordinateData {
            hash,
            index,
            prev: ArcSwapOption::empty(),
            next: ArcSwapOption::empty(),
        }))
    }

    /// Creates a new chain coordinate linked to a previous one.
    pub(crate) fn new_linked(hash: BlockHash, index: u64, prev: Option<ChainCoordinate>) -> Self {
        Self(std::sync::Arc::new(ChainCoordinateData {
            hash,
            index,
            prev: ArcSwapOption::new(prev.map(|c| c.0)),
            next: ArcSwapOption::empty(),
        }))
    }
}

impl ChainCoordinateData {
    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    /// Returns the sequential index.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Returns the previous coordinate in the chain.
    pub fn prev(&self) -> Option<ChainCoordinate> {
        self.prev.load_full().map(ChainCoordinate)
    }

    /// Returns the next coordinate in the chain.
    pub fn next(&self) -> Option<ChainCoordinate> {
        self.next.load_full().map(ChainCoordinate)
    }

    /// Sets the next coordinate.
    pub(crate) fn set_next(&self, next: Option<ChainCoordinate>) {
        self.next.store(next.map(|c| c.0));
    }

    /// Clears the previous pointer.
    pub(crate) fn clear_prev(&self) {
        self.prev.store(None);
    }
}

impl std::fmt::Debug for ChainCoordinate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainCoordinate")
            .field("hash", &self.hash)
            .field("index", &self.index)
            .finish()
    }
}
