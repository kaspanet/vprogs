use alloc::vec::Vec;

use borsh::BorshSerialize;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;

use super::{FIXED_HEADER_SIZE, RESOURCE_HEADER_SIZE};

/// Encodes a scheduler [`TransactionContext`](vprogs_scheduling_scheduler::TransactionContext) into
/// the ABI wire format.
pub fn encode<S, P>(ctx: &TransactionContext<'_, S, P>) -> Vec<u8>
where
    S: Store,
    P: Processor<BatchMetadata = ChainBlockMetadata>,
    P::Transaction: BorshSerialize,
{
    let chain_metadata = ctx.batch_metadata();
    let resources = ctx.resources();
    let tx_bytes = borsh::to_vec(ctx.tx()).expect("failed to serialize transaction");

    let resources_header = resources.len() * RESOURCE_HEADER_SIZE;
    let payload_len: usize = resources.iter().map(|r| r.data().len()).sum();
    let total = FIXED_HEADER_SIZE + tx_bytes.len() + resources_header + payload_len;

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
