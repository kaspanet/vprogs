use vprogs_l1_bridge::{RpcOptionalHeader, RpcOptionalTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::Processor;

/// Extension of [`Processor`] with L1 block pre-processing.
///
/// Implementors translate an L1 chain block (header + accepted transactions) into a batch of L2
/// transactions. This runs on an execution worker thread and may complete out of order relative to
/// block index.
pub trait NodeVm: Processor<BatchMetadata = ChainBlockMetadata> {
    /// Translates an L1 chain block into L2 transactions.
    ///
    /// Called on an execution worker thread. The returned transactions are later fed to the
    /// scheduler in index order.
    fn pre_process_block(
        &self,
        index: u64,
        header: &RpcOptionalHeader,
        accepted_transactions: &[RpcOptionalTransaction],
    ) -> Vec<Self::Transaction>;
}
