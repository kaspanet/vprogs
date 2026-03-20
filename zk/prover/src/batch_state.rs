use vprogs_zk_vm::ProofRequest;

/// Per-batch proving state.
pub(crate) struct BatchState<R> {
    /// Receipts indexed by tx_index. Pre-allocated to `expected_tx_count` slots.
    pub(crate) receipts: Vec<Option<(R, ProofRequest)>>,
    /// Number of receipts received so far.
    pub(crate) received: u32,
    /// Pre-batch resource IDs indexed by resource_index.
    pub(crate) resource_ids: Vec<[u8; 32]>,
}
