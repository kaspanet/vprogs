use kaspa_consensus_core::Hash;

/// Hash type alias for block hashes.
pub type BlockHash = Hash;

/// A new block was added to the L1 DAG.
#[derive(Clone, Debug)]
pub struct BlockAdded {
    /// Hash of the added block.
    pub block_hash: BlockHash,
    /// DAA score of the block.
    pub daa_score: u64,
    /// Blue score of the block.
    pub blue_score: u64,
    /// Timestamp of the block (milliseconds since epoch).
    pub timestamp: u64,
}

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

/// Finality conflict detected.
#[derive(Clone, Debug)]
pub struct FinalityConflict {
    /// Hash of the block causing the finality violation.
    pub violating_block_hash: BlockHash,
}

/// Finality conflict was resolved.
#[derive(Clone, Debug)]
pub struct FinalityResolved {
    /// Hash of the finality block.
    pub finality_block_hash: BlockHash,
}

/// Virtual DAA score changed.
#[derive(Clone, Debug)]
pub struct DaaScoreChanged {
    /// New virtual DAA score.
    pub daa_score: u64,
}

/// L1 event types emitted by the bridge.
#[derive(Clone, Debug)]
pub enum L1Event {
    /// Connection to L1 node established.
    Connected,
    /// Connection to L1 node lost.
    Disconnected,
    /// A new block was added to the DAG.
    BlockAdded(BlockAdded),
    /// Virtual chain changed (potential reorg).
    VirtualChainChanged(VirtualChainChanged),
    /// Finality conflict detected.
    FinalityConflict(FinalityConflict),
    /// Finality conflict resolved.
    FinalityResolved(FinalityResolved),
    /// DAA score changed.
    DaaScoreChanged(DaaScoreChanged),
}

impl L1Event {
    /// Returns true if this event indicates a potential reorg.
    pub fn is_reorg(&self) -> bool {
        matches!(self, L1Event::VirtualChainChanged(v) if v.is_reorg())
    }

    /// Returns true if this is a connection state change event.
    pub fn is_connection_event(&self) -> bool {
        matches!(self, L1Event::Connected | L1Event::Disconnected)
    }
}
