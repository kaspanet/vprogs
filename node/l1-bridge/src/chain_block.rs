use std::{
    fmt,
    fmt::{Debug, Formatter},
    sync::Arc,
};

use arc_swap::ArcSwapOption;
use kaspa_hashes::ZERO_HASH;
use tap::{Tap, TapOptional};
use vprogs_core_macros::smart_pointer;

use crate::BlockHash;

/// A block in the virtual chain, forming a doubly-linked list.
#[smart_pointer]
pub struct ChainBlock {
    /// L1 block hash.
    hash: BlockHash,
    /// Sequential index relative to the bridge's starting point.
    index: u64,
    /// Link to the preceding block, or empty if this is the root.
    prev: ArcSwapOption<ChainBlockData>,
    /// Link to the following block, or empty if this is the tip.
    next: ArcSwapOption<ChainBlockData>,
}

impl ChainBlock {
    /// Creates a standalone block with no links.
    pub fn new(hash: BlockHash, index: u64) -> Self {
        Self(Arc::new(ChainBlockData {
            hash,
            index,
            prev: ArcSwapOption::empty(),
            next: ArcSwapOption::empty(),
        }))
    }

    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    /// Returns the sequential index.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Appends a new block after this one and links them in both directions.
    pub(crate) fn advance_tip(&self, hash: BlockHash) -> Self {
        // Create the new block with a back-link to self.
        Self(Arc::new(ChainBlockData {
            hash,
            index: self.index + 1,
            prev: ArcSwapOption::new(Some(self.0.clone())),
            next: ArcSwapOption::empty(),
        }))
        .tap(|next| {
            // Complete the forward link from self to the new block.
            self.next.store(Some(next.0.clone()));
        })
    }

    /// Unlinks this block from the chain and returns its predecessor. Used during rollback to walk
    /// backwards from the tip.
    ///
    /// Panics if this block has no predecessor (i.e. it is the root).
    pub(crate) fn rollback_tip(&self) -> Self {
        // Take the prev pointer, clearing this block's back-link.
        self.prev.swap(None).map(Self).expect("tried to rollback root").tap(|prev| {
            // Clear the predecessor's forward link to fully unlink.
            prev.next.store(None);
        })
    }

    /// Unlinks this block from the chain and returns its successor. Used during finalization to
    /// walk forward from the root.
    pub(crate) fn advance_root(&self) -> Option<Self> {
        // Take the next pointer, clearing this block's forward link.
        self.next.swap(None).map(Self).tap_some(|next| {
            // Clear the successor's back-link to fully unlink.
            next.prev.store(None);
        })
    }
}

impl Default for ChainBlock {
    /// Returns a sentinel root (zero hash, index 0).
    fn default() -> Self {
        Self::new(ZERO_HASH, 0)
    }
}

impl Debug for ChainBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainBlock").field("hash", &self.hash).field("index", &self.index).finish()
    }
}
