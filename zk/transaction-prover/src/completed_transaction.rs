use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

use crate::TransactionBackend;

/// Result of a completed transaction proof, returned by the FuturesUnordered collection.
pub(crate) struct CompletedTransaction<P: Processor<S>, B: TransactionBackend, S: Store> {
    pub(crate) index: u32,
    pub(crate) batch: ScheduledBatch<S, P>,
    pub(crate) receipt: B::Receipt,
}
