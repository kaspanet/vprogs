use std::future::Future;

/// ZK backend for batch proving. Extends the transaction
/// [`Backend`](vprogs_zk_transaction_prover::Backend) with batch aggregation.
pub trait Backend: vprogs_zk_transaction_prover::Backend {
    /// Runs one batch prove attempt from the encoded witness, returning its receipt or the
    /// attempt's error (a prove error, a receipt the image id rejects, or an elapsed attempt).
    /// `receipts` are the inner transaction receipts that the batch guest verifies via
    /// composition. The calling worker owns the retry, so a persistent failure never wedges the
    /// prover's shutdown join.
    fn prove_batch(
        &self,
        inputs: &[u8],
        receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Result<Self::Receipt, String>> + Send + 'static;

    /// Extracts journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;

    /// Trusted batch-processor image id: the program identifier that keys a per-batch receipt in
    /// the proof-receipt store, and the trusted image the aggregator verifies each composed batch
    /// journal against.
    fn batch_image_id(&self) -> &[u8; 32];
}
