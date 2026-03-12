use alloc::vec::Vec;

use crate::{
    Parser, Result,
    transaction_processor::{BatchMetadata, Resource},
};

/// Decoded transaction inputs holding zero-copy views into the wire buffer.
pub struct Inputs<'a> {
    /// Borsh-serialized transaction bytes.
    pub tx: &'a [u8],
    /// Position of this transaction within the batch.
    pub tx_index: u32,
    /// Block-level metadata for the current batch.
    pub batch_metadata: BatchMetadata<'a>,
    /// Mutable resource views decoded from the wire buffer.
    pub resources: Vec<Resource<'a>>,
}

impl<'a> Inputs<'a> {
    /// Fixed header size: tx_index(4) + n_resources(4) + BatchMetadata + tx_bytes_len(4).
    pub const FIXED_HEADER_SIZE: usize = 4 + 4 + BatchMetadata::SIZE + 4;

    /// Decodes transaction inputs from the wire buffer.
    ///
    /// Wire layout: `fixed_header | tx_bytes | resource_headers | resource_data`
    pub fn decode(buf: &'a mut [u8]) -> Result<Self> {
        // Decode fixed header.
        let (header, data) = buf.split_at_mut(Self::FIXED_HEADER_SIZE);
        let tx_index = header[0..4].parse_u32("tx_index")?;
        let resource_count = header[4..8].parse_u32("resource_count")? as usize;
        let batch_metadata = BatchMetadata::decode(&header[8..])?;

        // Decode transaction bytes.
        let tx_length = header[8 + BatchMetadata::SIZE..].parse_u32("tx_length")? as usize;
        let (tx, resources) = data.split_at_mut(tx_length);

        // Decode resources.
        let resources_len = resource_count * Resource::HEADER_SIZE;
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
                BatchMetadata = vprogs_l1_types::ChainBlockMetadata,
            >,
        P::Transaction: borsh::BorshSerialize,
    {
        // Serialize transaction to bytes.
        let tx_bytes = borsh::to_vec(ctx.tx()).expect("failed to serialize transaction");

        // Calculate total size and allocate buffer.
        let res_header_size = ctx.resources().len() * Resource::HEADER_SIZE;
        let res_data_size: usize = ctx.resources().iter().map(|r| r.data().len()).sum();
        let total_size = Self::FIXED_HEADER_SIZE + tx_bytes.len() + res_header_size + res_data_size;
        let mut buf = Vec::with_capacity(total_size);

        // Write fixed header: tx_index, n_resources, batch metadata.
        buf.extend_from_slice(&ctx.tx_index().to_le_bytes());
        buf.extend_from_slice(&(ctx.resources().len() as u32).to_le_bytes());
        buf.extend_from_slice(&ctx.batch_metadata().block_hash().as_bytes());
        buf.extend_from_slice(&ctx.batch_metadata().blue_score().to_le_bytes());

        // Write transaction bytes (length-prefixed).
        buf.extend_from_slice(&(tx_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&tx_bytes);

        // Write resources headers.
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
            buf.extend_from_slice(r.data());
        }

        // Sanity check total size.
        debug_assert_eq!(buf.len(), total_size);

        buf
    }
}
