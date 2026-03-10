use alloc::vec::Vec;

use super::{batch_metadata::BatchMetadata, resource::Resource};

/// Decoded transaction input holding zero-copy views into the wire buffer.
pub struct Input<'a> {
    pub(crate) tx: &'a [u8],
    pub(crate) tx_index: u32,
    pub(crate) batch_metadata: BatchMetadata<'a>,
    pub(crate) resources: Vec<Resource<'a>>,
}

impl<'a> Input<'a> {
    /// Fixed header size: tx_index(4) + n_resources(4) + BatchMetadata + tx_bytes_len(4).
    pub const FIXED_HEADER_SIZE: usize = 4 + 4 + BatchMetadata::SIZE + 4;

    /// Decodes a transaction input from the wire buffer.
    pub(crate) fn decode(buf: &'a mut [u8]) -> Self {
        let tx_index = u32::from_le_bytes(buf[0..4].try_into().expect("truncated header"));
        let n_resources =
            u32::from_le_bytes(buf[4..8].try_into().expect("truncated header")) as usize;
        let tx_bytes_len = u32::from_le_bytes(
            buf[48..Self::FIXED_HEADER_SIZE].try_into().expect("truncated header"),
        ) as usize;

        let tx_bytes_end = Self::FIXED_HEADER_SIZE + tx_bytes_len;
        let resources_header_start = tx_bytes_end;
        let payload_start = resources_header_start + n_resources * Resource::HEADER_SIZE;

        let (header, mut payload) = buf.split_at_mut(payload_start);
        let header: &[u8] = header;

        let mut resources = Vec::with_capacity(n_resources);
        for i in 0..n_resources {
            resources.push(Resource::decode(
                &header[resources_header_start + i * Resource::HEADER_SIZE..],
                &mut payload,
            ));
        }

        let tx = &header[Self::FIXED_HEADER_SIZE..tx_bytes_end];
        let batch_metadata = BatchMetadata::decode(&header[8..48]);

        Self { tx, tx_index, batch_metadata, resources }
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
        let chain_metadata = ctx.batch_metadata();
        let resources = ctx.resources();
        let tx_bytes = borsh::to_vec(ctx.tx()).expect("failed to serialize transaction");

        let resources_header = resources.len() * Resource::HEADER_SIZE;
        let payload_len: usize = resources.iter().map(|r| r.data().len()).sum();
        let total = Self::FIXED_HEADER_SIZE + tx_bytes.len() + resources_header + payload_len;

        let mut buf = Vec::with_capacity(total);

        // Fixed header
        buf.extend_from_slice(&ctx.tx_index().to_le_bytes());
        buf.extend_from_slice(&(resources.len() as u32).to_le_bytes());
        buf.extend_from_slice(&chain_metadata.block_hash().as_bytes());
        buf.extend_from_slice(&chain_metadata.blue_score().to_le_bytes());
        buf.extend_from_slice(&(tx_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&tx_bytes);

        // Per-resource headers
        for r in resources {
            buf.extend_from_slice(r.access_metadata().resource_id.as_bytes());
            let flags = if r.is_new() { 1u8 } else { 0u8 };
            buf.push(flags);
            buf.extend_from_slice(&r.resource_index().to_le_bytes());
            buf.extend_from_slice(&(r.data().len() as u32).to_le_bytes());
        }

        // Payload
        for r in resources {
            buf.extend_from_slice(r.data());
        }

        debug_assert_eq!(buf.len(), total);
        buf
    }
}
