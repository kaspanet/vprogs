use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

/// A transaction submitted for proving. All metadata (index, resource IDs) is encoded in
/// `tx_inputs` and decoded via `Inputs::decode` on the prover thread.
pub struct Input<P: Processor<S>, S: Store> {
    pub batch: ScheduledBatch<S, P>,
    pub tx_inputs: Vec<u8>,
}
