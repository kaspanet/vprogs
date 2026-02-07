use vprogs_core_types::BatchMetadata;
use vprogs_node_l1_bridge::BlockHash;
use vprogs_scheduling_scheduler::{AccessHandle, RuntimeBatch, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_transaction_runtime::TransactionRuntime;
use vprogs_transaction_runtime_error::{VmError, VmResult};
use vprogs_transaction_runtime_object_access::ObjectAccess;
use vprogs_transaction_runtime_object_id::ObjectId;
use vprogs_transaction_runtime_transaction::Transaction;
use vprogs_transaction_runtime_transaction_effects::TransactionEffects;

#[derive(Default)]
pub struct BlockBatchMetadata(pub BlockHash);

impl BatchMetadata for BlockBatchMetadata {
    fn batch_id(&self) -> [u8; 32] {
        self.0.as_bytes()
    }
}

#[derive(Clone)]
pub struct VM;

impl VmInterface for VM {
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        tx: &Transaction,
        resources: &mut [AccessHandle<S, Self>],
    ) -> VmResult<TransactionEffects> {
        TransactionRuntime::execute(tx, resources)
    }

    fn notarize_batch<S: Store<StateSpace = StateSpace>>(&self, _batch: &RuntimeBatch<S, Self>) {}

    type Transaction = Transaction;
    type TransactionEffects = TransactionEffects;
    type ResourceId = ObjectId;
    type AccessMetadata = ObjectAccess;
    type BatchMetadata = BlockBatchMetadata;
    type Error = VmError;
}
