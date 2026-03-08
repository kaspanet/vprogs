/// Abstraction over a zero-knowledge VM backend (e.g. RISC-0, SP1).
///
/// Implementations own their ELF binaries and receive pre-encoded wire bytes
/// (produced by [`encode_transaction_context`](vprogs_zk_abi::host::encode_transaction_context)).
pub trait Backend: Clone + Send + Sync + 'static {
    /// The proof receipt type produced by this backend.
    type Receipt: Send + Sync + 'static;

    /// Execute a transaction from pre-encoded wire bytes.
    /// Returns the raw execution result bytes.
    fn execute_transaction(&self, wire_bytes: &[u8]) -> Vec<u8>;

    /// Prove a previously executed transaction from pre-encoded wire bytes.
    fn prove_transaction(&self, wire_bytes: &[u8]) -> Self::Receipt;

    /// Prove a batch of transactions from the encoded batch witness.
    fn prove_batch(&self, batch_witness: &[u8]) -> Self::Receipt;

    /// Extract journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
}
