use kaspa_hashes::Hash as BlockHash;

use crate::{
    chain_block::ChainBlock,
    error::{Error, Result},
};

/// Virtual chain tracking using a doubly-linked list of coordinates.
///
/// Supports O(1) append, O(n) rollback, and O(n) hash lookup by walking the list.
/// `root` represents the finalized/pruning threshold, `tip` is the latest
/// processed block. Both always point to a valid coordinate — on a fresh start, they point
/// to a sentinel root at index 0.
pub struct VirtualChain {
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
    pub fn new(root: ChainBlock) -> Self {
        Self { root: root.clone(), tip: root }
    }

    /// Returns the tip coordinate.
    pub fn tip(&self) -> ChainBlock {
        self.tip.clone()
    }

    /// Returns the root coordinate.
    pub fn root(&self) -> ChainBlock {
        self.root.clone()
    }

    /// Allocates the next index and records the block.
    pub fn add_block(&mut self, hash: BlockHash) -> u64 {
        self.tip = self.tip.append_next(hash);
        self.tip.index()
    }

    /// Rolls back by the given number of blocks, returning the new index.
    ///
    /// Returns `Ok(index)` on success, or `Err` if the rollback would go past `root`.
    pub fn rollback(&mut self, num_blocks: u64) -> Result<u64> {
        let root_index = self.root.index();

        for _ in 0..num_blocks {
            if self.tip.index() <= root_index {
                return Err(Error::RollbackPastRoot { num_blocks });
            }
            let prev = self.tip.prev().expect("non-root node must have prev");
            self.tip.clear_prev();
            prev.clear_next();
            self.tip = prev;
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
    pub fn advance_root(&mut self, hash: &BlockHash) -> Result<Option<ChainBlock>> {
        // Already at this pruning point.
        if self.root.hash() == *hash {
            return Ok(None);
        }

        // Walk forward, unlinking each node we pass.
        let mut current = self.root.next();
        self.root.clear_next();

        while let Some(coord) = current {
            if coord.hash() == *hash {
                coord.clear_prev();
                self.root = coord.clone();
                return Ok(Some(coord));
            }
            let next = coord.next();
            coord.clear_prev();
            coord.clear_next();
            current = next;
        }

        // Hash not found — chain is destroyed.
        Err(Error::HashNotFound(*hash))
    }
}
