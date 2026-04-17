use alloc::vec::Vec;

use vprogs_core_codec::Reader;

use crate::{
    Error, Result,
    transaction_processor::{BatchMetadata, Resource},
};

/// Decoded transaction inputs holding zero-copy views into the wire buffer.
pub struct Inputs<'a> {
    /// The L1 transaction's `payload` field, as-is. The guest hashes this with `PayloadDigest`
    /// to derive `payload_digest` for `tx_id` reconstruction. Guest programs that want the L2
    /// application slice must strip the borsh-encoded `Vec<AccessMetadata>` prefix themselves.
    pub payload: &'a [u8],
    /// Position of this transaction within the batch.
    pub tx_index: u32,
    /// Block-level metadata for the current batch.
    pub batch_metadata: BatchMetadata<'a>,
    /// Serialized L1 transaction fields excluding payload, signature scripts, and mass
    /// commitment. The guest hashes this with `TransactionRest` to derive `rest_digest`,
    /// and can parse it for input outpoints, outputs, etc.
    pub rest_preimage: &'a [u8],
    /// Mutable resource views decoded from the wire buffer.
    pub resources: Vec<Resource<'a>>,
}

impl<'a> Inputs<'a> {
    /// Fixed header size: tx_index(4) + n_resources(4) + BatchMetadata + rest_preimage_len(4)
    /// + payload_len(4).
    pub const FIXED_HEADER_SIZE: usize = 4 + 4 + BatchMetadata::SIZE + 4 + 4;

    /// Decodes transaction inputs from the wire buffer.
    ///
    /// Wire layout:
    /// `fixed_header | rest_preimage_bytes | payload_bytes | resource_headers | resource_data`
    pub fn decode(buf: &'a mut [u8]) -> Result<Self> {
        // Split fixed header from the rest of the buffer.
        let (header, data) = buf.split_at_mut(Self::FIXED_HEADER_SIZE);
        let mut header: &[u8] = header;

        // Decode fixed header.
        let tx_index = header.le_u32("tx_index")?;
        let resource_count = header.le_u32("resource_count")? as usize;
        let batch_metadata = BatchMetadata::decode(&mut header)?;
        let rest_preimage_len = header.le_u32("rest_preimage_length")? as usize;
        let payload_length = header.le_u32("payload_length")? as usize;

        // Decode rest preimage bytes.
        let (rest_preimage, data) = data.split_at_mut(rest_preimage_len);
        let rest_preimage: &[u8] = rest_preimage;

        // Decode payload bytes.
        let (payload, resources) = data.split_at_mut(payload_length);
        let payload: &[u8] = payload;

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

        Ok(Self { payload, tx_index, batch_metadata, rest_preimage, resources })
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
        use kaspa_consensus_core::hashing::tx::transaction_v1_rest_preimage;

        use crate::Write;

        let payload = ctx.tx().payload.as_slice();
        let rest_preimage = transaction_v1_rest_preimage(ctx.tx());

        // Calculate total size and allocate buffer.
        let res_header_size = ctx.resources().len() * Resource::HEADER_SIZE;
        let res_data_size: usize = ctx.resources().iter().map(|r| r.data().len()).sum();
        let total_size = Self::FIXED_HEADER_SIZE
            + rest_preimage.len()
            + payload.len()
            + res_header_size
            + res_data_size;
        let mut buf = Vec::with_capacity(total_size);

        // Write fixed header.
        buf.write(&ctx.tx_index().to_le_bytes());
        buf.write(&(ctx.resources().len() as u32).to_le_bytes());
        buf.write(&ctx.batch_metadata().block_hash().as_bytes());
        buf.write(&ctx.batch_metadata().blue_score().to_le_bytes());
        buf.write(&(rest_preimage.len() as u32).to_le_bytes());
        buf.write(&(payload.len() as u32).to_le_bytes());

        // Write rest preimage.
        buf.write(&rest_preimage);

        // Write payload bytes.
        buf.write(&payload);

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

        debug_assert_eq!(buf.len(), total_size);

        buf
    }
}
