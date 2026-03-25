/// Abstraction over the full ZK backend: synchronous execution plus transaction and batch proving.
///
/// Execution is synchronous (called on scheduler worker threads). Proving is async (handled by
/// background prover workers).
pub trait Backend: vprogs_zk_batch_prover::Backend {
    /// Execute a transaction from pre-encoded wire bytes.
    /// Returns the raw execution result bytes.
    fn execute_transaction(&self, wire_bytes: &[u8]) -> Vec<u8>;
}
