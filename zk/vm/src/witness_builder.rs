use vprogs_scheduling_scheduler::{AccessHandle, ProcessingContext};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::ZkVm;

/// Builds the flat wire-format witness buffer from transaction data and resource state.
///
/// Wire format (all integers little-endian, fields padded to 4-byte alignment):
/// ```text
/// [total_len: u32]
/// [tx_bytes_len: u32] [tx_bytes: u8*] [pad to align(4)]
/// [tx_index: u32]
/// [batch_metadata_len: u32] [batch_metadata: u8*] [pad to align(4)]
/// [num_accounts: u32]
///   for each account:
///     [account_id_len: u32] [account_id: u8*] [pad to align(4)]
///     [data_len: u32] [data: u8*] [pad to align(4)]
///     [version: u64]
/// ```
pub(crate) fn build_witness<S: Store<StateSpace = StateSpace>>(
    resources: &[AccessHandle<S, ZkVm>],
    ctx: &ProcessingContext<ZkVm>,
) -> Vec<u8> {
    let tx = ctx.transaction();
    let batch_metadata_bytes = borsh::to_vec(ctx.batch_metadata()).unwrap();

    let mut buf = Vec::new();
    // Reserve space for total_len (will be filled at the end).
    buf.extend_from_slice(&0u32.to_le_bytes());

    write_bytes_aligned(&mut buf, &tx.tx_bytes);
    buf.extend_from_slice(&ctx.tx_index().to_le_bytes());
    write_bytes_aligned(&mut buf, &batch_metadata_bytes);

    buf.extend_from_slice(&(resources.len() as u32).to_le_bytes());
    for resource in resources {
        let id_bytes = borsh::to_vec(&resource.access_metadata().id).unwrap();
        write_bytes_aligned(&mut buf, &id_bytes);
        write_bytes_aligned(&mut buf, resource.data());
        buf.extend_from_slice(&resource.version().to_le_bytes());
    }

    // Fill in total_len.
    let total_len = buf.len() as u32;
    buf[..4].copy_from_slice(&total_len.to_le_bytes());

    buf
}

fn write_bytes_aligned(buf: &mut Vec<u8>, data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(data);
    let rem = buf.len() % 4;
    if rem != 0 {
        buf.resize(buf.len() + (4 - rem), 0);
    }
}
