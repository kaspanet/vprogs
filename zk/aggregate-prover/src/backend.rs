use std::future::Future;

/// ZK backend for settlement-level aggregation. Extends the per-batch
/// [`Backend`](vprogs_zk_batch_prover::Backend) with aggregator proving, composing a bundle of
/// per-batch receipts into one settlement receipt.
pub trait Backend: vprogs_zk_batch_prover::Backend {
    /// Proves the aggregator from the encoded witness. `batch_receipts` are the per-batch receipts
    /// the aggregator guest verifies via composition. The returned receipt's journal is a
    /// [`StateTransition`](vprogs_zk_abi::batch_aggregator::StateTransition).
    fn prove_aggregator(
        &self,
        inputs: &[u8],
        batch_receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static;

    /// Aggregator image id: the program identifier that keys a settlement (bundle) receipt in the
    /// proof-receipt store. The trusted batch image the aggregator verifies its composed batch
    /// journals against is [`batch_image_id`](vprogs_zk_batch_prover::Backend::batch_image_id),
    /// inherited from the per-batch backend.
    fn aggregator_image_id(&self) -> &[u8; 32];
}
