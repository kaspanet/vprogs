use borsh::BorshDeserialize;
use vprogs_core_types::{AccessMetadata, AccessType};
use vprogs_node_framework::NodeVm;
use vprogs_node_l1_bridge::{ChainBlockMetadata, RpcOptionalHeader, RpcOptionalTransaction};
use vprogs_scheduling_scheduler::{AccessHandle, RuntimeBatch, VmInterface};
use vprogs_scheduling_test_suite::{Access, Tx};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

/// A minimal VM implementing [`NodeVm`] for testing the node framework.
#[derive(Clone)]
pub struct TestNodeVm;

impl NodeVm for TestNodeVm {
    fn pre_process_block(
        &self,
        _index: u64,
        _header: &RpcOptionalHeader,
        accepted_transactions: &[RpcOptionalTransaction],
    ) -> Vec<Tx> {
        accepted_transactions
            .iter()
            .filter_map(|l1_tx| Tx::try_from_slice(l1_tx.payload.as_ref()?).ok())
            .collect()
    }
}

impl VmInterface for TestNodeVm {
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

    fn post_process_batch<S: Store<StateSpace = StateSpace>>(&self, batch: &RuntimeBatch<S, Self>) {
        if !batch.was_canceled() {
            eprintln!(
                ">> Post Processed batch with {} transactions and {} state changes",
                batch.txs().len(),
                batch.state_diffs().len()
            );
        }
    }

    type Transaction = Tx;
    type TransactionEffects = ();
    type ResourceId = usize;
    type AccessMetadata = Access;
    type BatchMetadata = ChainBlockMetadata;
    type Error = ();
}
