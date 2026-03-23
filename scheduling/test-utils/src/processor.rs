use vprogs_core_types::AccessType;
use vprogs_scheduling_scheduler::TransactionContext;
use vprogs_storage_types::Store;

/// A minimal processor implementation for testing the scheduler.
#[derive(Clone)]
pub struct Processor;

impl<S: Store> vprogs_scheduling_scheduler::Processor<S> for Processor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        let tx_id = *ctx.tx();
        for resource in ctx.resources_mut() {
            if resource.access_metadata().access_type == AccessType::Write {
                resource.data_mut().extend_from_slice(&tx_id.to_be_bytes());
            }
        }
        Ok(())
    }

    type Transaction = usize;
    type TransactionEffects = ();
    type BatchMetadata = u64;
    type Error = ();
}
