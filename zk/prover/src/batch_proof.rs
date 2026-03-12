/// Result of proving an entire batch.
pub struct BatchProof {
    /// Hash of the block this batch belongs to.
    pub block_hash: [u8; 32],
    /// Sequential batch index.
    pub batch_index: u64,
    /// State root before this batch was applied.
    pub prev_root: [u8; 32],
    /// State root after this batch was applied.
    pub new_root: [u8; 32],
    /// Journal bytes from the batch proof receipt.
    pub receipt_journal: Vec<u8>,
}
