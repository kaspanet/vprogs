use alloc::vec::Vec;

use borsh::{BorshSerialize, io::Write};
use vprogs_core_types::ResourceId;

use crate::{
    ACCOUNT_HEADER_SIZE, FIXED_HEADER_SIZE, Result, account::Account, block_metadata::BlockMetadata,
};

/// Streams the execution result as a borsh-serialized `Result<Vec<Option<StorageOp>>, Error>`
/// to the writer. On success, each account serializes as the corresponding storage operation based
/// on its dirty/deleted/new flags. On error, writes the error.
pub fn write_execution_result<W: Write>(result: Result<&[Account<'_>]>, w: &mut W) {
    match result {
        Ok(accounts) => {
            1u8.serialize(w).expect("write failed"); // Borsh Result::Ok discriminant
            (accounts.len() as u32).serialize(w).expect("write failed");
            for account in accounts {
                account.serialize(w).expect("write failed");
            }
        }
        Err(err) => {
            0u8.serialize(w).expect("write failed"); // Borsh Result::Err discriminant
            err.serialize(w).expect("write failed");
        }
    }
}

/// Decodes a wire buffer into zero-copy block metadata and mutable account views.
///
/// The buffer is split into an immutable header region and a mutable payload region; each account
/// receives a disjoint `&mut [u8]` slice into the payload.
pub fn decode_transaction_context(
    buf: &mut [u8],
) -> (&[u8], u32, BlockMetadata<'_>, Vec<Account<'_>>) {
    let tx_index = u32::from_le_bytes(buf[0..4].try_into().expect("truncated header"));
    let n_accounts = u32::from_le_bytes(buf[4..8].try_into().expect("truncated header")) as usize;
    let blue_score = u64::from_le_bytes(buf[40..48].try_into().expect("truncated header"));
    let tx_bytes_len =
        u32::from_le_bytes(buf[48..FIXED_HEADER_SIZE].try_into().expect("truncated header"))
            as usize;

    let tx_bytes_end = FIXED_HEADER_SIZE + tx_bytes_len;
    let accounts_header_start = tx_bytes_end;
    let payload_start = accounts_header_start + n_accounts * ACCOUNT_HEADER_SIZE;

    // Split buffer: header part becomes immutable, payload part stays mutable.
    let (header, payload) = buf.split_at_mut(payload_start);
    let header: &[u8] = header;

    // Parse per-account headers and carve disjoint mutable payload slices in a single pass.
    let mut accounts = Vec::with_capacity(n_accounts);
    let mut remaining = &mut payload[..];
    for i in 0..n_accounts {
        let base = accounts_header_start + i * ACCOUNT_HEADER_SIZE;
        let rid_bytes: &[u8; 32] = header[base..base + 32].try_into().expect("truncated account");
        let resource_id = ResourceId::from_bytes_ref(rid_bytes);
        let is_new = header[base + 32] & 1 != 0;
        let data_len =
            u32::from_le_bytes(header[base + 33..base + 37].try_into().expect("truncated account"))
                as usize;
        let (slice, rest) = remaining.split_at_mut(data_len);
        remaining = rest;
        accounts.push(Account::new(resource_id, is_new, slice));
    }

    let block_hash: &[u8; 32] = header[8..40].try_into().expect("truncated header");
    let tx_bytes = &header[FIXED_HEADER_SIZE..tx_bytes_end];

    let block_metadata = BlockMetadata { block_hash, blue_score };
    (tx_bytes, tx_index, block_metadata, accounts)
}
