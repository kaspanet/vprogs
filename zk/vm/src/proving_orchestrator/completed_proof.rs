use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

use crate::ProofProvider;

/// Result of a completed transaction proof, returned by the FuturesUnordered collection.
pub(crate) struct CompletedTxProof<P: Processor<S>, Provider: ProofProvider, S: Store> {
    pub(crate) batch: ScheduledBatch<S, P>,
    pub(crate) tx_index: u32,
    pub(crate) receipt: Provider::Receipt,
}
