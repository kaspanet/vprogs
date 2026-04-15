use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

/// Command sent to the batch prover worker via its inbox.
pub(crate) enum Command<S: Store, P: Processor<S>> {
    /// A new batch is ready for proving.
    Batch(ScheduledBatch<S, P>),
    /// A rollback occurred; discard batches beyond the target index.
    Rollback(u64),
}
