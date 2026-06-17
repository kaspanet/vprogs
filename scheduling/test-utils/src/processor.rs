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
        let tx_id = ctx.scheduler_tx().tx;
        for resource in ctx.resources_mut() {
            if resource.access_metadata().access_type == AccessType::Write {
                resource.data_mut().extend_from_slice(&tx_id.to_be_bytes());
            }
        }
        Ok(())
    }

    // This processor never proves, so its image ids only need to be stable for receipt-cache
    // round-trips, not match any real program.
    fn tx_image_id(&self) -> [u8; 32] {
        [0u8; 32]
    }

    fn batch_image_id(&self) -> [u8; 32] {
        [1u8; 32]
    }

    fn aggregator_image_id(&self) -> [u8; 32] {
        [2u8; 32]
    }

    type Transaction = usize;
    type TransactionArtifact = Vec<u8>;
    type BatchArtifact = Vec<u8>;
    type AggregatorArtifact = Vec<u8>;
    type BatchMetadata = u64;
    type Error = ();
}
