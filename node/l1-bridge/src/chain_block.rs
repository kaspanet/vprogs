use std::{
    fmt,
    fmt::{Debug, Formatter},
    sync::Arc,
};

use arc_swap::ArcSwapOption;
use tap::{Tap, TapOptional};
use vprogs_core_macros::smart_pointer;

use crate::ChainBlockMetadata;

/// A block in the virtual chain, forming a doubly-linked list.
#[smart_pointer]
pub struct ChainBlock {
    /// Sequential index relative to the bridge's starting point.
    index: u64,
    /// Persistable metadata (hash, blue score).
    metadata: ChainBlockMetadata,
    /// Link to the preceding block, or empty if this is the root.
    prev: ArcSwapOption<ChainBlockData>,
    /// Link to the following block, or empty if this is the tip.
    next: ArcSwapOption<ChainBlockData>,
}

impl ChainBlock {
    /// Creates a standalone block with no links.
    pub fn new(index: u64, metadata: ChainBlockMetadata) -> Self {
        Self(Arc::new(ChainBlockData {
            index,
            metadata,
            prev: ArcSwapOption::empty(),
            next: ArcSwapOption::empty(),
        }))
    }

    /// Returns the sequential index.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Returns the persistable metadata (hash, blue score).
    pub fn metadata(&self) -> ChainBlockMetadata {
        self.metadata
    }

    /// Appends a new block after this one and links them in both directions.
    pub(crate) fn advance_tip(&self, metadata: ChainBlockMetadata) -> Self {
        // Create the new block with a back-link to self.
        Self(Arc::new(ChainBlockData {
            index: self.index + 1,
            metadata,
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

impl From<(u64, ChainBlockMetadata)> for ChainBlock {
    fn from((index, metadata): (u64, ChainBlockMetadata)) -> Self {
        Self::new(index, metadata)
    }
}

impl Default for ChainBlock {
    /// Returns a sentinel root (zero hash, index 0).
    fn default() -> Self {
        Self::new(0, ChainBlockMetadata::default())
    }
}

impl Debug for ChainBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainBlock")
            .field("index", &self.index)
            .field("metadata", &self.metadata)
            .finish()
    }
}
