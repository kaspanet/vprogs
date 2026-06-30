use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
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

impl<S: Store> Processor<S> for VM {
    fn process_transaction(&self, _ctx: &mut TransactionContext<S, Self>) -> VmResult<()> {
        // let (tx, resources) = ctx.parts_mut();
        // TransactionRuntime::execute(tx, resources)
        todo!("transaction execution from SchedulerTransaction<L1Transaction>")
    }

    // The node VM does not yet drive proving, so its receipt-cache image ids are unset.
    fn tx_image_id(&self) -> [u8; 32] {
        [0u8; 32]
    }

    fn batch_image_id(&self) -> [u8; 32] {
        [0u8; 32]
    }

    type Transaction = L1Transaction;
    type TransactionArtifact = TransactionEffects;
    type BatchArtifact = ();
    type AggregatorArtifact = ();
    type BatchMetadata = ChainBlockMetadata;
    type Error = VmError;
}
