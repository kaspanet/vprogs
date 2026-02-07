use vprogs_core_types::BatchMetadata;
use vprogs_node_framework::NodeVm;
use vprogs_node_l1_bridge::{BlockHash, ChainBlock, RpcOptionalHeader, RpcOptionalTransaction};
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
pub struct BlockBatchMetadata {
    pub hash: BlockHash,
    pub blue_score: u64,
}

impl BatchMetadata for BlockBatchMetadata {
    fn id(&self) -> [u8; 32] {
        self.hash.as_bytes()
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(40);
        bytes.extend_from_slice(&self.hash.as_bytes());
        bytes.extend_from_slice(&self.blue_score.to_be_bytes());
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self::default();
        }
        Self {
            hash: BlockHash::from_slice(&bytes[..32]),
            blue_score: u64::from_be_bytes(bytes[32..40].try_into().unwrap()),
        }
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

impl NodeVm for VM {
    fn pre_process_block(
        &self,
        _index: u64,
        header: &RpcOptionalHeader,
        _accepted_transactions: &[RpcOptionalTransaction],
    ) -> (Vec<Transaction>, BlockBatchMetadata) {
        let metadata = BlockBatchMetadata {
            hash: header.hash.unwrap_or_default(),
            blue_score: header.blue_score.unwrap_or_default(),
        };
        (vec![], metadata)
    }

    fn chain_block(&self, index: u64, metadata: &BlockBatchMetadata) -> ChainBlock {
        ChainBlock::new(metadata.hash, index, metadata.blue_score)
    }
}
