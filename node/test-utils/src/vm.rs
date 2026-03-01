use vprogs_core_types::AccessType;
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;

/// A minimal processor for testing the node framework.
#[derive(Clone)]
pub struct TestNodeVm;

impl Processor for TestNodeVm {
    fn process_transaction<S: Store>(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        let (tx, resources) = ctx.parts_mut();
        let tx_id_bytes = tx.id().as_bytes();
        for resource in resources {
            if resource.access_metadata().access_type == AccessType::Write {
                resource.data_mut().extend_from_slice(&tx_id_bytes);
            }
        }
        Ok(())
    }

    type Transaction = L1Transaction;
    type TransactionEffects = ();
    type BatchMetadata = ChainBlockMetadata;
    type Error = ();
}
