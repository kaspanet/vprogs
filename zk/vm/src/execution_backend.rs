/// Full ZK backend: synchronous execution plus transaction and batch proving.
pub trait Backend: vprogs_zk_batch_prover::Backend {
    /// Execute a transaction from pre-encoded wire bytes and return the raw result.
    fn execute_transaction(&self, wire_bytes: &[u8]) -> Vec<u8>;
}
