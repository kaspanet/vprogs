use kaspa_consensus_core::Hash as BlockHash;

/// Finality conflict was resolved.
#[derive(Clone, Debug)]
pub struct FinalityResolved {
    /// Hash of the finality block.
    pub finality_block_hash: BlockHash,
}
