use crate::{
    BlockHash,
    chain_block::ChainBlock,
    error::{Error, Result},
};

/// Virtual chain tracking using a doubly-linked list of coordinates.
///
/// Supports O(1) append, O(n) rollback, and O(n) hash lookup by walking the list.
/// `root` represents the finalized/pruning threshold, `tip` is the latest
/// processed block. Both always point to a valid coordinate — on a fresh start, they point
/// to a sentinel root at index 0.
pub(crate) struct VirtualChain {
    /// Finalized/pruning threshold (oldest block we track).
    root: ChainBlock,
    /// Latest processed block.
    tip: ChainBlock,
}

impl VirtualChain {
    /// Creates a new virtual chain rooted at the given coordinate.
    ///
    /// Both pointers start at the root. The caller advances `tip`
    /// by calling [`add_block`].
    pub(crate) fn new(root: ChainBlock) -> Self {
        Self { root: root.clone(), tip: root }
    }

    /// Returns the tip coordinate.
    pub(crate) fn tip(&self) -> ChainBlock {
        self.tip.clone()
    }

    /// Returns the root coordinate.
    pub(crate) fn root(&self) -> ChainBlock {
        self.root.clone()
    }

    /// Allocates the next index and records the block.
    pub(crate) fn add_block(&mut self, hash: BlockHash) -> u64 {
        self.tip = self.tip.attach(hash);
        self.tip.index()
    }

    /// Rolls back by the given number of blocks, returning the new index.
    ///
    /// Returns `Ok(index)` on success, or `Err` if the rollback would go past `root`.
    pub(crate) fn rollback(&mut self, num_blocks: u64) -> Result<u64> {
        if self.tip.index().saturating_sub(self.root.index()) < num_blocks {
            return Err(Error::RollbackPastRoot { num_blocks });
        }

        for _ in 0..num_blocks {
            self.tip = self.tip.rollback_tip();
        }

        Ok(self.tip.index())
    }

    /// Tries to advance the root to the given hash.
    ///
    /// Returns `Ok(Some(coord))` if the root advanced, `Ok(None)` if the hash matches the
    /// current `root` (no-op), or `Err` if the hash was not found in the chain.
    ///
    /// This walks forward from `root`, unlinking each node it passes. If the hash is
    /// not found, the chain is destroyed and the bridge must stop.
    pub(crate) fn advance_root(&mut self, hash: &BlockHash) -> Result<Option<ChainBlock>> {
        // Already at this pruning point.
        if self.root.hash() == *hash {
            return Ok(None);
        }

        // Walk forward, unlinking each node we pass.
        let mut current = self.root.advance_root();

        while let Some(coord) = current {
            if coord.hash() == *hash {
                self.root = coord.clone();
                return Ok(Some(coord));
            }
            current = coord.advance_root();
        }

        // Hash not found — chain is destroyed.
        Err(Error::HashNotFound(*hash))
    }
}
