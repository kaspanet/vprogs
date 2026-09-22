use alloc::vec::Vec;

use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
#[cfg(feature = "host")]
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
#[cfg(feature = "host")]
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
#[cfg(feature = "host")]
use vprogs_storage_types::Store;
#[cfg(feature = "host")]
use zerocopy::{IntoBytes, little_endian::U64};

use crate::{
    Result,
    transaction_processor::{MergesetContext, Resource, Transaction},
};

/// Per-tx execution data.
pub struct ExecutionInput<'a> {
    /// Mergeset context preimage, exposed to the VM as a source of on-chain randomness.
    pub context: &'a MergesetContext,
    /// Transaction to execute.
    pub tx: Transaction<'a>,
    /// Mutable resource views.
    pub resources: Vec<Resource<'a>>,
}

impl<'a> ExecutionInput<'a> {
    /// Decodes an execution input from the wire buffer.
    pub fn decode(mut buf: &'a mut [u8]) -> Result<Self> {
        let context = buf.array_as::<MergesetContext>("context")?;
        let tx = Transaction::decode(&mut buf)?;
        let res_iter = tx.payload.access_metadata.iter().map(|am| Resource::decode(&mut buf, am));
        Ok(Self { context, tx, resources: res_iter.collect::<Result<_>>()? })
    }

    /// Encodes an execution input to the wire buffer.
    #[cfg(feature = "host")]
    pub fn encode<S, P>(buf: &mut Vec<u8>, ctx: &TransactionContext<'_, S, P>)
    where
        S: Store,
        P: Processor<S, Transaction = L1Transaction, BatchMetadata = ChainBlockMetadata>,
    {
        buf.write(
            MergesetContext {
                timestamp: U64::new(ctx.batch_metadata().prev_timestamp),
                daa_score: U64::new(ctx.batch_metadata().daa_score),
                blue_score: U64::new(ctx.batch_metadata().blue_score),
            }
            .as_bytes(),
        );
        Transaction::encode(buf, &ctx.scheduler_tx().tx);
        for r in ctx.resources() {
            Resource::encode(buf, r);
        }
    }
}
