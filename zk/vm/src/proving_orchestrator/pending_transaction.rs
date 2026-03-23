use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

/// A transaction submitted for proving. All metadata (index, resource IDs) is encoded in
/// `input_bytes` and decoded via `Inputs::decode` on the prover thread.
pub(crate) struct PendingTransaction<P: Processor<S>, S: Store> {
    pub(crate) batch: ScheduledBatch<S, P>,
    pub(crate) input_bytes: Vec<u8>,
}
