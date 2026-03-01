use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};

/// Convenience bound for a [`Processor`](vprogs_scheduling_scheduler::Processor) whose
/// transaction and batch-metadata types are pinned to the L1 types used by the node framework.
pub trait Processor:
    vprogs_scheduling_scheduler::Processor<
        Transaction = L1Transaction,
        BatchMetadata = ChainBlockMetadata,
    >
{
}

impl<
    P: vprogs_scheduling_scheduler::Processor<
            Transaction = L1Transaction,
            BatchMetadata = ChainBlockMetadata,
        >,
> Processor for P
{
}
