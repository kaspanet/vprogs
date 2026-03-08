/// Result of proving an entire batch.
pub struct BatchProof {
    pub block_hash: [u8; 32],
    /// Journal bytes for each transaction, ordered by tx_index.
    pub inner_receipts: Vec<Vec<u8>>,
}
