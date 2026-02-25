/// Result of proving an entire batch.
pub struct BatchProof {
    pub batch_index: u64,
    /// Journal bytes for each transaction, ordered by tx_index.
    pub inner_receipts: Vec<Vec<u8>>,
}
