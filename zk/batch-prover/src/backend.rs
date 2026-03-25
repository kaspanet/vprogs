use std::future::Future;

/// ZK backend for batch proving. Extends the transaction
/// [`Backend`](vprogs_zk_transaction_prover::Backend) with batch aggregation.
pub trait Backend: vprogs_zk_transaction_prover::Backend {
    /// Proves a batch from the encoded witness. `receipts` are the inner transaction receipts
    /// that the batch guest verifies via composition.
    fn prove_batch(
        &self,
        inputs: &[u8],
        receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static;

    /// Extracts journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
}
