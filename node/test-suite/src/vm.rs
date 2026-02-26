use borsh::BorshDeserialize;
use vprogs_core_types::{AccessMetadata, AccessType};
use vprogs_l1_bridge::{RpcOptionalHeader, RpcOptionalTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_node_framework::NodeVm;
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_scheduling_test_suite::{Access, Tx};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

/// A minimal processor implementing [`NodeVm`] for testing the node framework.
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

impl Processor for TestNodeVm {
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
    type BatchMetadata = ChainBlockMetadata;
    type Error = ();
}
