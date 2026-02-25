use vprogs_scheduling_scheduler::TransactionContext;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_types::{Account, Witness};

use crate::{Backend, Vm};

/// Extension trait for snapshotting a [`TransactionContext`] into rkyv-serialized witness bytes.
pub trait TransactionContextExt {
    fn witness(&self) -> Vec<u8>;
}

impl<S: Store<StateSpace = StateSpace>, B: Backend> TransactionContextExt
    for TransactionContext<'_, S, Vm<B>>
{
    fn witness(&self) -> Vec<u8> {
        let witness = Witness {
            tx_bytes: self.transaction().tx_bytes.clone(),
            tx_index: self.tx_index(),
            batch_metadata: borsh::to_vec(self.batch_metadata()).unwrap(),
            accounts: self
                .resources()
                .iter()
                .map(|r| Account {
                    account_id: borsh::to_vec(&r.access_metadata().id).unwrap(),
                    data: r.data().clone(),
                    version: r.version(),
                })
                .collect(),
        };
        rkyv::to_bytes::<rkyv::rancor::Error>(&witness).unwrap().to_vec()
    }
}
