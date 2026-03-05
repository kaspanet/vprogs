use vprogs_zk_abi::StorageOp;

use crate::Result;

/// Abstraction over a zero-knowledge VM backend (e.g. RISC-0, SP1).
///
/// Implementations own their ELF binaries and receive pre-encoded wire bytes
/// (produced by [`TransactionContext::encode`](vprogs_zk_abi::TransactionContext::encode)).
pub trait Backend: Clone + Send + Sync + 'static {
    /// The proof receipt type produced by this backend.
    type Receipt: Send + Sync + 'static;

    /// Execute a transaction from pre-encoded wire bytes.
    /// Returns one optional state op per account.
    fn execute_transaction(&self, wire_bytes: &[u8]) -> Result<Vec<Option<StorageOp>>>;

    /// Prove a previously executed transaction from pre-encoded wire bytes.
    fn prove_transaction(&self, wire_bytes: &[u8]) -> Result<Self::Receipt>;

    /// Prove a batch of transactions.
    fn prove_batch(&self, block_hash: [u8; 32], journals: &[Vec<u8>]) -> Result<Self::Receipt>;

    /// Extract journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
}
