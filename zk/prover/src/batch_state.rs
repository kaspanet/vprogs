use vprogs_zk_vm::ProofRequest;

/// Per-batch proving state - accumulates receipts and resource IDs until the batch is complete.
pub(crate) struct BatchState<R> {
    /// Receipts indexed by tx_index, pre-allocated to `expected_tx_count` slots.
    pub(crate) receipts: Vec<Option<(R, ProofRequest)>>,
    /// Number of receipts received so far.
    pub(crate) received: u32,
    /// Resource IDs indexed by resource_index.
    pub(crate) resource_ids: Vec<[u8; 32]>,
}
