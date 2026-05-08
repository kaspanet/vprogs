use alloc::vec::Vec;

use kaspa_hashes::Hash;
use vprogs_core_codec::Reader;

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
        if tx.payload().access_metadata.len() != resource_count {
            return Err(Error::Decode("access_metadata/resource_count mismatch".into()));
        }

        // Sanity check that header offsets do not overflow.
        let resources_len = resource_count
            .checked_mul(Resource::HEADER_SIZE)
            .ok_or_else(|| Error::Decode("resource_count overflow".into()))?;

        // Decode resources, pairing each with its access_metadata entry by position.
        let (res_headers, mut res_data) = resources.split_at_mut(resources_len);
        let mut resources = Vec::with_capacity(resource_count);
        for (i, access_metadata) in tx.payload().access_metadata.iter().enumerate() {
            resources.push(Resource::decode(
                &res_headers[i * Resource::HEADER_SIZE..],
                access_metadata,
                &mut res_data,
            )?);
        }

        Ok(Self { context_hash, tx, resources })
    }
}
