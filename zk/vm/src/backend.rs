/// Raw execution outcome of one transaction: the guest's stdout stream plus the journal it
/// committed.
pub struct ExecOutcome {
    /// Bytes the guest wrote to stdout.
    pub stdout: Vec<u8>,
    /// Journal bytes the guest committed; carries the
    /// [`OutputCommitment`](vprogs_zk_abi::transaction_processor::OutputCommitment), including
    /// any emitted exits.
    pub journal: Vec<u8>,
}

/// Full ZK backend: synchronous execution plus transaction, batch, and settlement-aggregation
/// proving. Extending the aggregate backend lets the VM expose every program image id (including
/// the aggregator's) through its [`Processor`](vprogs_scheduling_scheduler::Processor) impl.
pub trait Backend: vprogs_zk_aggregate_prover::Backend {
    /// Execute a transaction from pre-encoded wire bytes, returning its stdout stream and its
    /// committed journal.
    fn execute_transaction(&self, wire_bytes: &[u8]) -> ExecOutcome;
}
