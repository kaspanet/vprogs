use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_storage_types::Store;

/// Convenience bound for a [`Processor`](vprogs_scheduling_scheduler::Processor) whose
/// transaction and batch-metadata types are pinned to the L1 types used by the node framework.
pub trait Processor<S: Store>:
    vprogs_scheduling_scheduler::Processor<
        S,
        Transaction = L1Transaction,
        BatchMetadata = ChainBlockMetadata,
    >
{
}

impl<
    S: Store,
    P: vprogs_scheduling_scheduler::Processor<
            S,
            Transaction = L1Transaction,
            BatchMetadata = ChainBlockMetadata,
        >,
> Processor<S> for P
{
}
