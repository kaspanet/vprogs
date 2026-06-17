/// Full ZK backend: synchronous execution plus transaction, batch, and settlement-aggregation
/// proving. Extending the aggregate backend lets the VM expose every program image id (including
/// the aggregator's) through its [`Processor`](vprogs_scheduling_scheduler::Processor) impl.
pub trait Backend: vprogs_zk_aggregate_prover::Backend {
    /// Execute a transaction from pre-encoded wire bytes and return the raw result.
    fn execute_transaction(&self, wire_bytes: &[u8]) -> Vec<u8>;
}
