use vprogs_node_l1_bridge::{ChainBlockMetadata, RpcOptionalHeader, RpcOptionalTransaction};
use vprogs_scheduling_scheduler::VmInterface;

/// Extension of [`VmInterface`] with L1 block pre-processing.
///
/// Implementors translate an L1 chain block (header + accepted transactions) into a batch of L2
/// transactions. This runs on an execution worker thread and may complete out of order relative to
/// block index.
pub trait NodeVm: VmInterface<BatchMetadata = ChainBlockMetadata> {
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
