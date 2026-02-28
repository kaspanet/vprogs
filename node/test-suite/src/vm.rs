use vprogs_core_types::{AccessType, L2Transaction};
use vprogs_l1_bridge::RpcOptionalHeader;
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_node_framework::NodeVm;
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;

/// A minimal processor implementing [`NodeVm`] for testing the node framework.
#[derive(Clone)]
pub struct TestNodeVm;

impl NodeVm for TestNodeVm {
    fn pre_process_block(
        &self,
        _index: u64,
        _header: &RpcOptionalHeader,
        accepted_transactions: &[L1Transaction],
    ) -> Vec<L2Transaction<Self::L1Transaction>> {
        accepted_transactions
            .iter()
            .filter_map(|l1_tx| {
                Some(L2Transaction {
                    l1_tx: l1_tx.clone(),
                    resources: borsh::from_slice(&l1_tx.payload).ok()?,
                })
            })
            .collect()
    }
}

impl Processor for TestNodeVm {
    fn process_transaction<S: Store>(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        let (tx, resources) = ctx.parts_mut();
        let tx_id_bytes = tx.l1_tx.id().as_bytes();
        for resource in resources {
            if resource.access_metadata().access_type == AccessType::Write {
                resource.data_mut().extend_from_slice(&tx_id_bytes);
            }
        }
        Ok(())
    }

    type L1Transaction = L1Transaction;
    type TransactionEffects = ();
    type BatchMetadata = ChainBlockMetadata;
    type Error = ();
}
