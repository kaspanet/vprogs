use kaspa_consensus_core::Hash as BlockHash;

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
