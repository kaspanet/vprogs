use std::fmt;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use kaspa_hashes::{Hash as BlockHash, ZERO_HASH};
use tap::{Tap, TapOptional};
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

    /// Creates and attaches a new block after this one, returning it.
    pub(crate) fn attach(&self, hash: BlockHash) -> Self {
        Self(Arc::new(ChainBlockData {
            hash,
            index: self.index + 1,
            prev: ArcSwapOption::new(Some(self.0.clone())),
            next: ArcSwapOption::empty(),
        }))
        .tap(|next| {
            self.next.store(Some(next.0.clone()));
        })
    }

    /// Unlinks this block from its predecessor and returns the predecessor.
    ///
    /// Panics if this block has no predecessor (i.e. it is the root).
    pub(crate) fn rollback_tip(&self) -> Self {
        self.prev.swap(None).map(Self).expect("tried to rollback root").tap(|prev| {
            prev.next.store(None);
        })
    }

    /// Unlinks this block from its successor and returns it.
    /// The returned successor's prev pointer is cleared.
    pub(crate) fn advance_root(&self) -> Option<Self> {
        self.next.swap(None).map(Self).tap_some(|next| {
            next.prev.store(None);
        })
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
}

impl Default for ChainBlock {
    /// Creates a sentinel root block (ZERO_HASH, index 0, no links).
    fn default() -> Self {
        Self::new(ZERO_HASH, 0)
    }
}

impl Debug for ChainBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainBlock").field("hash", &self.hash).field("index", &self.index).finish()
    }
}
