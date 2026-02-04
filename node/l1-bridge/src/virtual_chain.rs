use crate::{
    BlockHash,
    chain_block::ChainBlock,
    error::{Error, Result},
};

/// Tracks the virtual chain as a doubly-linked list between a finalized `root` and the latest
/// processed `tip`.
pub(crate) struct VirtualChain {
    /// Finalization boundary — blocks at or before this point are considered final.
    root: ChainBlock,
    /// Most recently processed block.
    tip: ChainBlock,
}

impl VirtualChain {
    /// Creates a new chain where both root and tip point to the given block.
    pub(crate) fn new(root: ChainBlock) -> Self {
        Self { root: root.clone(), tip: root }
    }

    /// Returns the current tip.
    pub(crate) fn tip(&self) -> ChainBlock {
        self.tip.clone()
    }

    /// Returns the current root.
    pub(crate) fn root(&self) -> ChainBlock {
        self.root.clone()
    }

    /// Appends a block at the tip and returns its index.
    pub(crate) fn advance_tip(&mut self, hash: BlockHash) -> u64 {
        self.tip = self.tip.advance_tip(hash);
        self.tip.index()
    }

    /// Rolls back `num_blocks` from the tip and returns the new tip index. Returns an error if the
    /// rollback would go past the root.
    pub(crate) fn rollback(&mut self, num_blocks: u64) -> Result<u64> {
        // Ensure we don't roll back past the finalization boundary.
        let target_index = self.tip.index().saturating_sub(num_blocks);
        if target_index < self.root.index() {
            return Err(Error::RollbackPastRoot { target_index, root_index: self.root.index() });
        }

        // Walk backwards, unlinking each block from its predecessor.
        for _ in 0..num_blocks {
            self.tip = self.tip.rollback_tip();
        }

        Ok(self.tip.index())
    }

    /// Advances the root forward to the block matching `hash`, unlinking all nodes it passes.
    /// Returns the new root, `None` if already there, or an error if the hash is not found (which
    /// destroys the chain — fatal).
    pub(crate) fn advance_root(&mut self, hash: &BlockHash) -> Result<Option<ChainBlock>> {
        // Already at this pruning point — nothing to do.
        if self.root.hash() == *hash {
            return Ok(None);
        }

        // Walk forward from root, unlinking each node until we find the target.
        let mut current = self.root.advance_root();
        while let Some(block) = current {
            if block.hash() == *hash {
                self.root = block.clone();
                return Ok(Some(block));
            }
            current = block.advance_root();
        }

        // The target hash was not found — the chain is now destroyed and the bridge must stop.
        Err(Error::HashNotFound(*hash))
    }
}
