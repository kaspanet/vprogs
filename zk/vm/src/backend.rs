use vprogs_zk_types::{StateOp, TransactionContextWitness};

/// Errors returned by ZK backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    Failed(String),
}

/// Abstraction over a zero-knowledge VM backend (e.g. RISC-0, SP1).
///
/// Implementations own their ELF binaries and receive a typed [`TransactionContextWitness`].
/// The associated [`Receipt`](Backend::Receipt) type is backend-specific.
pub trait Backend: Clone + Send + Sync + 'static {
    /// The proof receipt type produced by this backend.
    type Receipt: Send + Sync + 'static;

    /// Execute a transaction from a witness snapshot. Returns one optional state op per account.
    fn execute(&self, witness: &TransactionContextWitness) -> Result<Vec<Option<StateOp>>, BackendError>;

    /// Prove a previously executed transaction from its witness.
    fn prove_transaction(&self, witness: &TransactionContextWitness) -> Result<Self::Receipt, BackendError>;

    /// Prove a batch of transactions.
    fn prove_batch(
        &self,
        batch_index: u64,
        journals: &[Vec<u8>],
    ) -> Result<Self::Receipt, BackendError>;

    /// Extract journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
}
