use kaspa_consensus_core::Hash as BlockHash;

/// Finality conflict detected.
#[derive(Clone, Debug)]
pub struct FinalityConflict {
    /// Hash of the block causing the finality violation.
    pub violating_block_hash: BlockHash,
}
