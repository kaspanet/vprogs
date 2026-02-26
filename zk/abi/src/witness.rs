use alloc::vec::Vec;

use rkyv::{Archive, Serialize};

use crate::{Account, BatchMetadata};

/// Owned witness data for passing to ZK backends.
///
/// Serialized once via rkyv; the guest accesses the archived form zero-copy.
#[derive(Archive, Serialize)]
pub struct Witness {
    pub tx_bytes: Vec<u8>,
    pub tx_index: u32,
    pub batch_metadata: BatchMetadata,
    pub accounts: Vec<Account>,
}

#[cfg(feature = "host")]
mod from_ctx {
    use vprogs_l1_types::ChainBlockMetadata;
    use vprogs_scheduling_scheduler::{Processor, TransactionContext};
    use vprogs_state_space::StateSpace;
    use vprogs_storage_types::Store;

    use super::*;
    use crate::{AccessMetadata, Transaction};

    impl<S, P> From<&TransactionContext<'_, S, P>> for Witness
    where
        S: Store<StateSpace = StateSpace>,
        P: Processor<
                Transaction = Transaction,
                AccessMetadata = AccessMetadata,
                BatchMetadata = ChainBlockMetadata,
            >,
    {
        fn from(ctx: &TransactionContext<'_, S, P>) -> Self {
            let chain_metadata = ctx.batch_metadata();
            Witness {
                tx_bytes: ctx.transaction().tx_bytes.clone(),
                tx_index: ctx.tx_index(),
                batch_metadata: BatchMetadata {
                    block_hash: chain_metadata.hash().as_bytes(),
                    blue_score: chain_metadata.blue_score(),
                },
                accounts: ctx
                    .resources()
                    .iter()
                    .map(|r| Account {
                        account_id: borsh::to_vec(&r.access_metadata().id).unwrap(),
                        data: r.data().clone(),
                    })
                    .collect(),
            }
        }
    }
}
