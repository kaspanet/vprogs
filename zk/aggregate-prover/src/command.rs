use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

/// Command sent to the aggregate prover worker via its inbox.
pub(crate) enum Command<S: Store, P: Processor<S>> {
    /// A new batch is ready; accumulate it for bundling.
    Batch(ScheduledBatch<S, P>),
    /// A rollback occurred; discard queued batches with index beyond the target.
    Rollback(u64),
}
