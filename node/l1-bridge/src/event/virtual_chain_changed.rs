use kaspa_consensus_core::Hash as BlockHash;

/// Virtual chain changed (reorg event).
#[derive(Clone, Debug)]
pub struct VirtualChainChanged {
    /// Block hashes removed from the virtual chain (in order from tip to common ancestor).
    pub removed_block_hashes: Vec<BlockHash>,
    /// Block hashes added to the virtual chain (in order from common ancestor to new tip).
    pub added_block_hashes: Vec<BlockHash>,
}

impl VirtualChainChanged {
    /// Returns true if this event represents a reorg (blocks were removed).
    pub fn is_reorg(&self) -> bool {
        !self.removed_block_hashes.is_empty()
    }
}
