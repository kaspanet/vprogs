use vprogs_core_types::{AccessMetadata, AccessType};
use vprogs_scheduling_scheduler::{TransactionContext, TransactionProcessor};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{Access, Tx};

/// A minimal VM implementation for testing the scheduler.
#[derive(Clone)]
pub struct VM;

impl TransactionProcessor for VM {
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        let tx_id = ctx.transaction().0;
        for resource in ctx.resources_mut() {
            if resource.access_metadata().access_type() == AccessType::Write {
                resource.data_mut().extend_from_slice(&tx_id.to_be_bytes());
            }
        }
        Ok(())
    }

    type Transaction = Tx;
    type TransactionEffects = ();
    type ResourceId = usize;
    type AccessMetadata = Access;
    type BatchMetadata = u64;
    type Error = ();
}
