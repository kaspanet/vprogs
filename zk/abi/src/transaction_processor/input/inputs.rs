use alloc::vec::Vec;

use vprogs_core_codec::Reader;

use crate::{
    Error, Result,
    transaction_processor::{BatchMetadata, Resource, Transaction},
};

/// Decoded transaction inputs holding zero-copy views into the wire buffer.
pub struct Inputs<'a> {
    /// Transaction to execute.
    pub tx: Transaction<'a>,
    /// Position of this transaction within the batch.
    pub tx_index: u32,
    /// Block-level metadata for the current batch.
    pub batch_metadata: BatchMetadata<'a>,
    /// Mutable resource views decoded from the wire buffer.
    pub resources: Vec<Resource<'a>>,
}

impl<'a> Inputs<'a> {
    /// Fixed header size: tx_index(4) + n_resources(4) + BatchMetadata.
    pub const FIXED_HEADER_SIZE: usize = 4 + 4 + BatchMetadata::SIZE;

    /// Decodes transaction inputs from the wire buffer.
    ///
    /// Wire layout: `fixed_header | tx_bytes | resource_headers | resource_data`.
    pub fn decode(buf: &'a mut [u8]) -> Result<Self> {
        // Split fixed header from the rest of the buffer, creating mutable view for resource data.
        let (header, data) = buf.split_at_mut(Self::FIXED_HEADER_SIZE);
        let mut header: &[u8] = header;

        // Decode fixed header.
        let tx_index = header.le_u32("tx_index")?;
        let resource_count = header.le_u32("resource_count")? as usize;
        let batch_metadata = BatchMetadata::decode(&mut header)?;

        // Decode transaction bytes.
        let (tx_bytes, resources) = data.split_at_mut(Transaction::wire_size(data)?);
        let tx = Transaction::decode(&mut &*tx_bytes)?;

        // Sanity check that header offsets do not overflow.
        let resources_len = resource_count
            .checked_mul(Resource::HEADER_SIZE)
            .ok_or_else(|| Error::Decode("resource_count overflow".into()))?;

        // Decode resources.
        let (res_headers, mut res_data) = resources.split_at_mut(resources_len);
        let mut resources = Vec::with_capacity(resource_count);
        for i in 0..resource_count {
            resources
                .push(Resource::decode(&res_headers[i * Resource::HEADER_SIZE..], &mut res_data)?);
        }

        Ok(Self { tx, tx_index, batch_metadata, resources })
    }

    /// Encodes a scheduler [`TransactionContext`] into the ABI wire format (host-side only).
    #[cfg(feature = "host")]
    pub fn encode<S, P>(ctx: &vprogs_scheduling_scheduler::TransactionContext<'_, S, P>) -> Vec<u8>
    where
        S: vprogs_storage_types::Store,
        P: vprogs_scheduling_scheduler::Processor<
                S,
                Transaction = vprogs_l1_types::L1Transaction,
                BatchMetadata = vprogs_l1_types::ChainBlockMetadata,
            >,
    {
        use crate::Write;

        // Pre-allocate buffer: fixed header, resource headers, resource data. The transaction
        // envelope size depends on per-version preimage derivation, so it grows the buffer.
        let res_header_size = ctx.resources().len() * Resource::HEADER_SIZE;
        let res_data_size: usize = ctx.resources().iter().map(|r| r.data().len()).sum();
        let mut buf = Vec::with_capacity(Self::FIXED_HEADER_SIZE + res_header_size + res_data_size);

        // Write fixed header: tx_index, n_resources, batch metadata.
        buf.write(&ctx.tx_index().to_le_bytes());
        buf.write(&(ctx.resources().len() as u32).to_le_bytes());
        buf.write(&ctx.batch_metadata().block_hash().as_bytes());
        buf.write(&ctx.batch_metadata().blue_score().to_le_bytes());

        // Write the transaction envelope (dispatches on tx.version).
        Transaction::encode(&mut buf, ctx.tx());

        // Write resource headers.
        for r in ctx.resources() {
            Resource::encode_header(
                &mut buf,
                &r.access_metadata().resource_id,
                r.is_new(),
                r.resource_index(),
                r.data().len() as u32,
            );
        }

        // Write resource data.
        for r in ctx.resources() {
            buf.write(r.data());
        }

        buf
    }
}
