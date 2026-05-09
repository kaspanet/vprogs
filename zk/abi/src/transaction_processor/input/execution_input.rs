use alloc::vec::Vec;

use kaspa_hashes::Hash;
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
    /// Fixed header size: resource_count(4) + context_hash(32).
    pub const FIXED_HEADER_SIZE: usize = 4 + 32;

    /// Decodes an execution input from the wire buffer.
    ///
    /// Wire layout: `fixed_header | tx_bytes | resource_headers | resource_data`.
    pub fn decode(buf: &'a mut [u8]) -> Result<Self> {
        // Split fixed header from the rest of the buffer, creating mutable view for resource data.
        let (header, data) = buf.split_at_mut(Self::FIXED_HEADER_SIZE);
        let mut header: &[u8] = header;

        // Decode fixed header.
        let resource_count = header.le_u32("resource_count")? as usize;
        let context_hash = header.array_as::<Hash>("context_hash")?;

        // Decode transaction bytes (immutably reborrowed so access metadata can borrow from them).
        let (tx_bytes, resources) = data.split_at_mut(Transaction::wire_size(data)?);
        let tx = Transaction::decode(&mut &*tx_bytes)?;
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

    /// Returns the wire size of the resource portion (header + data) plus the fixed header.
    /// Excludes the transaction envelope, whose size depends on per-version preimage derivation
    /// and is appended at encode time.
    #[cfg(feature = "host")]
    pub fn wire_size<S, P>(ctx: &TransactionContext<'_, S, P>) -> usize
    where
        S: Store,
        P: Processor<S, Transaction = L1Transaction, BatchMetadata = ChainBlockMetadata>,
    {
        if ctx.scheduler_tx().tx.version != Transaction::SUPPORTED_VERSION {
            return 0;
        }

        let res_header_total = ctx.resources().len() * Resource::HEADER_SIZE;
        let res_data_total: usize = ctx.resources().iter().map(|r| r.data().len()).sum();

        Self::FIXED_HEADER_SIZE + res_header_total + res_data_total
    }

    /// Encodes the execution-input portion of the wire envelope from a scheduler
    /// [`TransactionContext`] (host-side only).
    ///
    /// Wire layout: `resource_count | context_hash | tx_bytes | resource_headers | resource_data`.
    #[cfg(feature = "host")]
    pub fn encode<S, P>(buf: &mut Vec<u8>, ctx: &TransactionContext<'_, S, P>)
    where
        S: Store,
        P: Processor<S, Transaction = L1Transaction, BatchMetadata = ChainBlockMetadata>,
    {
        // Fixed header: resource_count, context_hash (derived from batch metadata).
        let bm = ctx.batch_metadata();
        let context_hash = kaspa_seq_commit::hashing::mergeset_context_hash(
            &kaspa_seq_commit::types::MergesetContext {
                timestamp: kaspa_seq_commit::hashing::seq_commit_timestamp(bm.prev_timestamp),
                daa_score: bm.daa_score,
                blue_score: bm.blue_score,
            },
        );
        buf.write(&(ctx.resources().len() as u32).to_le_bytes());
        buf.write(context_hash.as_slice());

        // Transaction envelope.
        Transaction::encode(buf, &ctx.scheduler_tx().tx);

        // Resource headers, then resource data.
        for r in ctx.resources() {
            Resource::encode_header(buf, r.is_new(), r.resource_index(), r.data().len() as u32);
        }
        for r in ctx.resources() {
            buf.write(r.data());
        }
    }
}
