use vprogs_scheduling_scheduler::{Processor, ScheduledTransaction};
use vprogs_storage_types::Store;

/// A transaction submitted for proving. All metadata (index, resource IDs) is encoded in
/// `tx_inputs` and decoded via `Inputs::decode` on the prover thread.
pub struct Input<P: Processor<S>, S: Store> {
    pub tx: ScheduledTransaction<S, P>,
    pub tx_inputs: Vec<u8>,
}
