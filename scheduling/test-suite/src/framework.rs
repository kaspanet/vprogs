use std::sync::Arc;

use vprogs_core_types::{AccessMetadata, AccessType, Transaction};
use vprogs_scheduling_scheduler::{AccessHandle, RuntimeBatch, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::{ReadStore, Store};

/// A minimal VM implementation for testing the scheduler.
#[derive(Clone)]
pub struct TestVM;

impl VmInterface for TestVM {
    type Transaction = Tx;
    type TransactionEffects = ();
    type ResourceId = usize;
    type AccessMetadata = Access;
    type Error = ();

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
}

/// A test transaction that writes its ID to each accessed resource.
pub struct Tx(pub usize, pub Vec<Access>);

impl Transaction<usize, Access> for Tx {
    fn accessed_resources(&self) -> &[Access] {
        &self.1
    }
}

/// A test access metadata specifying read or write access to a resource.
#[derive(Clone)]
pub enum Access {
    Read(usize),
    Write(usize),
}

impl AccessMetadata<usize> for Access {
    fn id(&self) -> usize {
        match self {
            Access::Read(id) | Access::Write(id) => *id,
        }
    }

    fn access_type(&self) -> AccessType {
        match self {
            Access::Read(_) => AccessType::Read,
            Access::Write(_) => AccessType::Write,
        }
    }
}

/// Asserts that a resource has the expected version and writer log.
pub struct AssertWrittenState(pub usize, pub Vec<usize>);

impl AssertWrittenState {
    pub fn assert<S: ReadStore<StateSpace = StateSpace>>(&self, store: &Arc<S>) {
        let writer_count = self.1.len();
        let writer_log: Vec<u8> = self.1.iter().flat_map(|id| id.to_be_bytes()).collect();

        let versioned_state = StateVersion::<usize>::from_latest_data(store.as_ref(), self.0);
        assert_eq!(versioned_state.version(), writer_count as u64);
        assert_eq!(*versioned_state.data(), writer_log);
    }
}

/// Asserts that a resource has been deleted (no latest pointer exists).
pub struct AssertResourceDeleted(pub usize);

impl AssertResourceDeleted {
    pub fn assert<S: ReadStore<StateSpace = StateSpace>>(&self, store: &Arc<S>) {
        let id_bytes = self.0.to_be_bytes();
        assert!(
            store.get(StateSpace::StatePtrLatest, &id_bytes).is_none(),
            "Resource {} should have been deleted but still exists",
            self.0
        );
    }
}
