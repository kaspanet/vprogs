use vprogs_core_types::Checkpoint;

use crate::{
    BlockHash, ChainBlockMetadata,
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
    /// Creates a new chain where both root and tip point to the given checkpoint.
    pub(crate) fn new(checkpoint: Checkpoint<ChainBlockMetadata>) -> Self {
        let root = ChainBlock::new(checkpoint);
        Self { root: root.clone(), tip: root }
    }

    /// Returns the checkpoint for the current tip.
    pub(crate) fn tip(&self) -> Checkpoint<ChainBlockMetadata> {
        self.tip.checkpoint().clone()
    }

    /// Returns the checkpoint for the current root.
    pub(crate) fn root(&self) -> Checkpoint<ChainBlockMetadata> {
        self.root.checkpoint().clone()
    }

    /// Appends a checkpoint at the tip and returns it.
    pub(crate) fn advance_tip(
        &mut self,
        metadata: ChainBlockMetadata,
    ) -> Checkpoint<ChainBlockMetadata> {
        self.tip = self.tip.advance_tip(metadata);
        self.tip.checkpoint().clone()
    }

    /// Rolls back `num_checkpoints` from the tip and returns the new tip checkpoint along with the
    /// blue score depth (difference between old and new tip). Returns an error if the rollback
    /// would go past the root.
    pub(crate) fn rollback(
        &mut self,
        num_checkpoints: u64,
    ) -> Result<(Checkpoint<ChainBlockMetadata>, u64)> {
        // Ensure we don't roll back past the finalization boundary.
        let target_index = self.tip.index().saturating_sub(num_checkpoints);
        if target_index < self.root.index() {
            return Err(Error::RollbackPastRoot { target_index, root_index: self.root.index() });
        }

        // Calculate reorg depth and walk backwards, unlinking each node from its predecessor.
        let old_blue_score = self.tip.metadata().blue_score();
        for _ in 0..num_checkpoints {
            self.tip = self.tip.rollback_tip();
        }
        let blue_score_depth = old_blue_score.saturating_sub(self.tip.metadata().blue_score());

        Ok((self.tip.checkpoint().clone(), blue_score_depth))
    }

    /// Advances the root forward to the checkpoint matching `hash`, unlinking all nodes it passes.
    /// Returns the new root checkpoint, `None` if already there, or an error if the hash is not
    /// found (which destroys the chain - fatal).
    pub(crate) fn advance_root(
        &mut self,
        hash: &BlockHash,
    ) -> Result<Option<Checkpoint<ChainBlockMetadata>>> {
        // Already at this pruning point — nothing to do.
        if self.root.metadata().hash() == *hash {
            return Ok(None);
        }

        // Walk forward from root, unlinking each node until we find the target.
        let mut current = self.root.advance_root();
        while let Some(block) = current {
            if block.metadata().hash() == *hash {
                self.root = block;
                return Ok(Some(self.root.checkpoint().clone()));
            }
            current = block.advance_root();
        }

        // The target hash was not found — the chain is now destroyed and the bridge must stop.
        Err(Error::HashNotFound(*hash))
    }
}

impl Drop for VirtualChain {
    /// Walks from root to tip, breaking Arc cycles between adjacent chain nodes.
    fn drop(&mut self) {
        let mut current = self.root.advance_root();
        while let Some(block) = current {
            current = block.advance_root();
        }
    }
}
