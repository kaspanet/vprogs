use alloc::vec::Vec;

#[cfg(feature = "host")]
use kaspa_consensus_core::hashing::tx::transaction_v1_rest_preimage;
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
    Error, Result,
    transaction_processor::{Resource, Transaction},
};

/// Per-tx execution data. Present in [`Inputs`](crate::transaction_processor::Inputs) only when
/// the version is supported by this prover build.
pub struct ExecutionInput<'a> {
    /// Mergeset context hash - exposed to the VM as a source of on-chain randomness.
    pub context_hash: &'a Hash,
    /// Transaction to execute.
    pub tx: Transaction<'a>,
    /// Mutable resource views decoded from the wire buffer.
    pub resources: Vec<Resource<'a>>,
}

impl<'a> ExecutionInput<'a> {
    /// Fixed header size: resource_count(4) + context_hash(32) + tx_size(4).
    pub const FIXED_HEADER_SIZE: usize = 4 + 32 + 4;

    /// Decodes an execution input from the wire buffer.
    ///
    /// Wire layout: `fixed_header | tx_bytes (tx_size) | resource_headers | resource_data`.
    pub fn decode(buf: &'a mut [u8]) -> Result<Self> {
        // Split fixed header from the rest of the buffer, creating mutable view for resource data.
        let (header, data) = buf.split_at_mut(Self::FIXED_HEADER_SIZE);
        let mut header: &[u8] = header;

        // Decode fixed header.
        let resource_count = header.le_u32("resource_count")? as usize;
        let context_hash = header.array_as::<Hash>("context_hash")?;
        let tx_size = header.le_u32("tx_size")? as usize;

        // Split off the transaction bytes; the rest is the resource section.
        let (tx_bytes, resources) = data.split_at_mut(tx_size);
        let tx = Transaction::decode(tx_bytes)?;
        if tx.payload.access_metadata.len() != resource_count {
            return Err(Error::Decode("access_metadata/resource_count mismatch".into()));
        }

        // Sanity check that header offsets do not overflow.
        let resources_len = resource_count
            .checked_mul(Resource::HEADER_SIZE)
            .ok_or_else(|| Error::Decode("resource_count overflow".into()))?;

        // Decode resources, pairing each with its access_metadata entry by position.
        let (res_headers, mut res_data) = resources.split_at_mut(resources_len);
        let mut resources = Vec::with_capacity(resource_count);
        for (i, access_metadata) in tx.payload.access_metadata.iter().enumerate() {
            resources.push(Resource::decode(
                &res_headers[i * Resource::HEADER_SIZE..],
                access_metadata,
                &mut res_data,
            )?);
        }

        Ok(Self { context_hash, tx, resources })
    }

    /// Encodes the execution-input portion of the wire envelope from a [`TransactionContext`].
    #[cfg(feature = "host")]
    pub fn encode<S, P>(buf: &mut Vec<u8>, ctx: &TransactionContext<'_, S, P>)
    where
        S: Store,
        P: Processor<S, Transaction = L1Transaction, BatchMetadata = ChainBlockMetadata>,
    {
        // Derive context_hash and rest_preimage; compute tx_size up-front for the header.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: ctx.batch_metadata().prev_timestamp,
            daa_score: ctx.batch_metadata().daa_score,
            blue_score: ctx.batch_metadata().blue_score,
        });
        let rest_preimage = transaction_v1_rest_preimage(&ctx.scheduler_tx().tx);
        let tx_size = (4 + ctx.scheduler_tx().tx.payload.len() + 4 + rest_preimage.len()) as u32;

        // Fixed header: resource_count, context_hash, tx_size.
        buf.write(&(ctx.resources().len() as u32).to_le_bytes());
        buf.write(context_hash.as_slice());
        buf.write(&tx_size.to_le_bytes());

        // Transaction bytes.
        Transaction::encode(buf, &ctx.scheduler_tx().tx.payload, &rest_preimage);

        // Resource headers, then resource data.
        for r in ctx.resources() {
            Resource::encode_header(buf, r.is_new(), r.resource_index(), r.data().len() as u32);
        }
        for r in ctx.resources() {
            buf.write(r.data());
        }
    }
}
