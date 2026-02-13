use std::{
    fmt,
    fmt::{Debug, Formatter},
    sync::Arc,
};

use arc_swap::ArcSwapOption;
use tap::{Tap, TapOptional};
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::Checkpoint;

use crate::ChainBlockMetadata;

/// A block in the virtual chain - a [`Checkpoint`] with doubly-linked list pointers.
#[smart_pointer(deref = checkpoint)]
pub struct ChainBlock {
    /// Persistable state (index + metadata).
    checkpoint: Checkpoint<ChainBlockMetadata>,
    /// Link to the preceding block, or empty if this is the root.
    prev: ArcSwapOption<ChainBlockData>,
    /// Link to the following block, or empty if this is the tip.
    next: ArcSwapOption<ChainBlockData>,
}

impl ChainBlock {
    /// Creates a standalone block with no links.
    pub fn new(checkpoint: Checkpoint<ChainBlockMetadata>) -> Self {
        Self(Arc::new(ChainBlockData {
            checkpoint,
            prev: ArcSwapOption::empty(),
            next: ArcSwapOption::empty(),
        }))
    }

    /// Returns the persistable checkpoint (index + metadata).
    pub fn checkpoint(&self) -> &Checkpoint<ChainBlockMetadata> {
        &self.checkpoint
    }

    /// Appends a new block after this one and links them in both directions.
    pub(crate) fn advance_tip(&self, metadata: ChainBlockMetadata) -> Self {
        // Create the new block with a back-link to self.
        Self(Arc::new(ChainBlockData {
            checkpoint: Checkpoint::new(self.index() + 1, metadata),
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
        Self::new(Checkpoint::default())
    }
}

impl Debug for ChainBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainBlock").field("checkpoint", &self.checkpoint).finish()
    }
}
