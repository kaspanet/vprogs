use vprogs_scheduling_scheduler::TransactionContext;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_types::{AccountInput, Witness};

use crate::{Backend, Vm};

/// Extension trait for snapshotting a [`TransactionContext`] into an owned [`Witness`].
pub trait IntoWitness {
    fn witness(&self) -> Witness;
}

impl<S: Store<StateSpace = StateSpace>, B: Backend> IntoWitness
    for TransactionContext<'_, S, Vm<B>>
{
    fn witness(&self) -> Witness {
        Witness {
            tx_bytes: self.transaction().tx_bytes.clone(),
            tx_index: self.tx_index(),
            batch_metadata: borsh::to_vec(self.batch_metadata()).unwrap(),
            accounts: self
                .resources()
                .iter()
                .map(|r| AccountInput {
                    account_id: borsh::to_vec(&r.access_metadata().id).unwrap(),
                    data: r.data().clone(),
                    version: r.version(),
                })
                .collect(),
        }
    }
}
