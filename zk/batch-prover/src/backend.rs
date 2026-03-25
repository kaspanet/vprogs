use std::future::Future;

/// Abstraction over a zero-knowledge backend's batch proving capabilities.
///
/// Extends [`vprogs_zk_transaction_prover::Backend`] because batch proving requires transaction
/// receipts (journal extraction, image IDs) produced by the transaction prover.
pub trait Backend: vprogs_zk_transaction_prover::Backend {
    /// The future type returned by batch proving.
    type BatchProveFuture: Future<Output = Self::Receipt> + Send + 'static;

    /// Prove a batch of transactions from the encoded batch witness.
    ///
    /// `receipts` contains the inner transaction receipts that the batch guest will verify
    /// via composition.
    fn prove_batch(&self, inputs: &[u8], receipts: Vec<Self::Receipt>) -> Self::BatchProveFuture;
}
