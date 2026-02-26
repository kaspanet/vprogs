use vprogs_l1_bridge::{RpcOptionalHeader, RpcOptionalTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_node_framework::NodeVm;
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_transaction_runtime::TransactionRuntime;
use vprogs_transaction_runtime_error::{VmError, VmResult};
use vprogs_transaction_runtime_object_access::ObjectAccess;
use vprogs_transaction_runtime_object_id::ObjectId;
use vprogs_transaction_runtime_transaction::Transaction;
use vprogs_transaction_runtime_transaction_effects::TransactionEffects;

/// Concrete processor backed by the transaction runtime.
///
/// Delegates transaction execution to [`TransactionRuntime`] and serves as the production
/// [`Processor`] used by the node.
#[derive(Clone)]
pub struct VM;

impl Processor for VM {
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> VmResult<TransactionEffects> {
        let (tx, resources) = ctx.parts_mut();
        TransactionRuntime::execute(tx, resources)
    }

    type Transaction = Transaction;
    type TransactionEffects = TransactionEffects;
    type ResourceId = ObjectId;
    type AccessMetadata = ObjectAccess;
    type BatchMetadata = ChainBlockMetadata;
    type Error = VmError;
}

/// Stub implementation — returns an empty transaction vec for now.
impl NodeVm for VM {
    fn pre_process_block(
        &self,
        _index: u64,
        _header: &RpcOptionalHeader,
        _accepted_transactions: &[RpcOptionalTransaction],
    ) -> Vec<Transaction> {
        vec![]
    }
}
