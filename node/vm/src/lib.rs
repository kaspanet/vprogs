use vprogs_core_types::L2Transaction;
use vprogs_l1_bridge::RpcOptionalHeader;
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_node_framework::NodeVm;
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_transaction_runtime_error::{VmError, VmResult};
use vprogs_transaction_runtime_transaction_effects::TransactionEffects;

/// Concrete processor backed by the transaction runtime.
///
/// Delegates transaction execution to [`TransactionRuntime`] and serves as the production
/// [`Processor`] used by the node.
#[derive(Clone)]
pub struct VM;

impl Processor for VM {
    fn process_transaction<S: Store>(
        &self,
        _ctx: &mut TransactionContext<S, Self>,
    ) -> VmResult<TransactionEffects> {
        todo!("transaction execution from L2Transaction<L1Transaction>")
    }

    type L1Transaction = L1Transaction;
    type TransactionEffects = TransactionEffects;
    type BatchMetadata = ChainBlockMetadata;
    type Error = VmError;
}

/// Stub implementation — returns an empty transaction vec for now.
impl NodeVm for VM {
    fn pre_process_block(
        &self,
        _index: u64,
        _header: &RpcOptionalHeader,
        _accepted_transactions: &[L1Transaction],
    ) -> Vec<L2Transaction<Self::L1Transaction>> {
        vec![]
    }
}
