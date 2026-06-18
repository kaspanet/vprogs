use alloc::vec::Vec;

use kaspa_hashes::Hash;
#[cfg(feature = "host")]
use kaspa_seq_commit::{hashing::mergeset_context_hash, types::MergesetContext};
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
#[cfg(feature = "host")]
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
#[cfg(feature = "host")]
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
#[cfg(feature = "host")]
use vprogs_storage_types::Store;

use crate::{
    Result,
    transaction_processor::{Resource, Transaction},
};

/// Per-tx execution data.
pub struct ExecutionInput<'a> {
    /// Mergeset context hash, exposed to the VM as a source of on-chain randomness.
    pub context_hash: &'a Hash,
    /// Transaction to execute.
    pub tx: Transaction<'a>,
    /// Mutable resource views.
    pub resources: Vec<Resource<'a>>,
}

impl<'a> ExecutionInput<'a> {
    /// Decodes an execution input from the wire buffer.
    pub fn decode(mut buf: &'a mut [u8]) -> Result<Self> {
        let context_hash = buf.array_as::<Hash>("context_hash")?;
        let tx = Transaction::decode(&mut buf)?;
        let res_iter = tx.payload.access_metadata.iter().map(|am| Resource::decode(&mut buf, am));
        Ok(Self { context_hash, tx, resources: res_iter.collect::<Result<_>>()? })
    }

    /// Encodes an execution input to the wire buffer.
    #[cfg(feature = "host")]
    pub fn encode<S, P>(buf: &mut Vec<u8>, ctx: &TransactionContext<'_, S, P>)
    where
        S: Store,
        P: Processor<S, Transaction = L1Transaction, BatchMetadata = ChainBlockMetadata>,
    {
        buf.write(
            mergeset_context_hash(&MergesetContext {
                timestamp: ctx.batch_metadata().prev_timestamp,
                daa_score: ctx.batch_metadata().daa_score,
                blue_score: ctx.batch_metadata().blue_score,
            })
            .as_slice(),
        );
        Transaction::encode(buf, &ctx.scheduler_tx().tx);
        for r in ctx.resources() {
            Resource::encode(buf, r);
        }
    }
}
