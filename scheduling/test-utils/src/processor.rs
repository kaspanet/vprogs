use vprogs_core_types::AccessType;
use vprogs_scheduling_scheduler::TransactionContext;
use vprogs_storage_types::Store;

/// A minimal processor implementation for testing the scheduler.
///
/// A write transaction appends its id to each resource it writes, except the sentinel
/// [`Processor::CLEAR_DATA`], which empties (removes) the resource instead.
#[derive(Clone)]
pub struct Processor;

impl Processor {
    /// Transaction id whose writes empty (remove) the resource rather than appending to it.
    pub const CLEAR_DATA: usize = usize::MAX;
}

impl<S: Store> vprogs_scheduling_scheduler::Processor<S> for Processor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        let tx_id = ctx.scheduler_tx().tx;
        for resource in ctx.resources_mut() {
            if resource.access_metadata().access_type == AccessType::Write {
                if tx_id == Self::CLEAR_DATA {
                    resource.set_data(Vec::new());
                } else {
                    resource.data_mut().extend_from_slice(&tx_id.to_be_bytes());
                }
            }
        }
        Ok(())
    }

    type Transaction = usize;
    type TransactionArtifact = ();
    type BatchArtifact = ();
    type BatchMetadata = u64;
    type Error = ();
}
