use vprogs_core_types::{AccessMetadata, AccessType};
use vprogs_scheduling_scheduler::{AccessHandle, RuntimeBatch, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{Access, Transaction};

/// A minimal VM implementation for testing the scheduler.
#[derive(Clone)]
pub struct VM;

impl VmInterface for VM {
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        tx: &Self::Transaction,
        resources: &mut [AccessHandle<S, Self>],
    ) -> Result<(), Self::Error> {
        for resource in resources {
            if resource.access_metadata().access_type() == AccessType::Write {
                resource.data_mut().extend_from_slice(&tx.0.to_be_bytes());
            }
        }
        Ok(())
    }

    fn notarize_batch<S: Store<StateSpace = StateSpace>>(&self, batch: &RuntimeBatch<S, Self>) {
        eprintln!(
            ">> Processed batch with {} transactions and {} state changes",
            batch.txs().len(),
            batch.state_diffs().len()
        );
    }

    type Transaction = Transaction;
    type TransactionEffects = ();
    type ResourceId = usize;
    type AccessMetadata = Access;
    type Error = ();
}
