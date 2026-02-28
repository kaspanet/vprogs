use alloc::vec::Vec;

use rkyv::{Archive, Serialize};

use crate::{Account, BatchMetadata};

/// ABI-level transaction context for passing to ZK backends.
///
/// Serialized once via rkyv; the guest accesses the archived form zero-copy.
#[derive(Clone, Debug, Archive, Serialize)]
pub struct TransactionContext {
    pub tx_bytes: Vec<u8>,
    pub tx_index: u32,
    pub batch_metadata: BatchMetadata,
    pub accounts: Vec<Account>,
}

#[cfg(feature = "host")]
mod from_ctx {
    use vprogs_l1_types::ChainBlockMetadata;
    use vprogs_scheduling_scheduler::{Processor, TransactionContext};
    use vprogs_storage_types::Store;

    use super::*;

    impl<S, P> From<&TransactionContext<'_, S, P>> for super::TransactionContext
    where
        S: Store,
        P: Processor<BatchMetadata = ChainBlockMetadata>,
        P::Transaction: borsh::BorshSerialize,
    {
        fn from(ctx: &TransactionContext<'_, S, P>) -> Self {
            let chain_metadata = ctx.batch_metadata();
            super::TransactionContext {
                tx_bytes: borsh::to_vec(&ctx.transaction().inner).unwrap(),
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
