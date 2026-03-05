//! Zero-copy transaction context: the ABI wire format between host and guest.
//!
//! Wire layout:
//! ```text
//! HEADER (immutable on guest side):
//!   [u32]  tx_index
//!   [u32]  n_accounts
//!   [u8;32] block_hash
//!   [u64]  blue_score
//!   [u32]  tx_bytes_len
//!   [u8; tx_bytes_len]  tx_bytes
//!   Per account i in 0..n_accounts:
//!     [u8;32] resource_id
//!     [u8]    flags        (bit 0 = is_new)
//!     [u32]   data_len
//!
//! PAYLOAD (mutable on guest side):
//!   [u8; data_len_0]  account_data_0
//!   [u8; data_len_1]  account_data_1
//!   ...
//! ```

use alloc::vec::Vec;

use borsh::BorshSerialize;
use vprogs_core_types::ResourceId;

use crate::{account::Account, metadata::Metadata, storage_op::StorageOpRef};

/// Size of the per-account header entry: 32 (resource_id) + 1 (flags) + 4 (data_len) = 37.
const ACCOUNT_HEADER_SIZE: usize = 32 + 1 + 4;

/// Zero-copy transaction context: metadata + mutable account views.
///
/// On the host side, [`encode`](TransactionContext::encode) serializes a scheduler
/// [`TransactionContext`](vprogs_scheduling_scheduler::TransactionContext) into the ABI wire
/// format. On the guest side, [`decode`](TransactionContext::decode) parses that blob into
/// borrowed slices — account data lives directly in the buffer with no copies.
pub struct TransactionContext<'a> {
    pub metadata: Metadata<'a>,
    pub accounts: Vec<Account<'a>>,
}

impl TransactionContext<'_> {
    /// Streams borsh-serialized `Vec<Option<StorageOpRef>>` to the writer without cloning
    /// account data. The output is wire-compatible with `Vec<Option<StorageOp>>`.
    pub fn write_storage_ops<W: borsh::io::Write>(&self, w: &mut W) -> borsh::io::Result<()> {
        (self.accounts.len() as u32).serialize(w)?;

        for account in &self.accounts {
            if account.is_deleted() {
                let op: Option<StorageOpRef> = Some(StorageOpRef::Delete);
                op.serialize(w)?;
            } else if account.is_dirty() {
                let data = account.data();
                let op = if account.is_new() {
                    StorageOpRef::Create(data)
                } else {
                    StorageOpRef::Update(data)
                };
                let op: Option<StorageOpRef> = Some(op);
                op.serialize(w)?;
            } else {
                let op: Option<StorageOpRef> = None;
                op.serialize(w)?;
            }
        }
        Ok(())
    }

    /// Decodes a wire buffer into a zero-copy `TransactionContext`.
    ///
    /// The buffer is split into an immutable header and a mutable payload region. Each account
    /// receives a disjoint `&mut [u8]` slice into the payload.
    pub fn decode(buf: &mut [u8]) -> TransactionContext<'_> {
        Self::decode_and_observe(buf, |_| {})
    }

    /// Decodes while feeding each buffer section to `observe` exactly once.
    ///
    /// This lets the caller hash (or otherwise process) the wire bytes in the same pass as
    /// decoding, avoiding a separate traversal of the buffer.
    pub fn decode_and_observe(
        buf: &mut [u8],
        mut observe: impl FnMut(&[u8]),
    ) -> TransactionContext<'_> {
        // --- Fixed header (52 bytes) ---
        observe(&buf[..52]);
        let tx_index = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let n_accounts = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        let block_hash_bytes: [u8; 32] = buf[8..40].try_into().unwrap();
        let blue_score = u64::from_le_bytes(buf[40..48].try_into().unwrap());
        let tx_bytes_len = u32::from_le_bytes(buf[48..52].try_into().unwrap()) as usize;

        let tx_bytes_end = 52 + tx_bytes_len;
        let accounts_header_start = tx_bytes_end;
        let accounts_header_end = accounts_header_start + n_accounts * ACCOUNT_HEADER_SIZE;
        let payload_start = accounts_header_end;

        // --- Variable header (tx_bytes + per-account headers) ---
        observe(&buf[52..payload_start]);

        let mut account_descs = Vec::with_capacity(n_accounts);
        for i in 0..n_accounts {
            let base = accounts_header_start + i * ACCOUNT_HEADER_SIZE;
            let rid_bytes: [u8; 32] = buf[base..base + 32].try_into().unwrap();
            let flags = buf[base + 32];
            let data_len =
                u32::from_le_bytes(buf[base + 33..base + 37].try_into().unwrap()) as usize;
            account_descs.push((ResourceId::from(rid_bytes), flags & 1 != 0, data_len));
        }

        // --- Payload (account data) — observed only, never parsed ---
        observe(&buf[payload_start..]);

        // Split buffer: header part becomes immutable, payload part stays mutable.
        let (header, payload) = buf.split_at_mut(payload_start);
        let header: &[u8] = header;
        let tx_bytes = &header[52..tx_bytes_end];

        // Carve disjoint mutable slices out of the payload.
        let mut accounts = Vec::with_capacity(n_accounts);
        let mut remaining = &mut payload[..];
        for (resource_id, is_new, data_len) in account_descs {
            let (slice, rest) = remaining.split_at_mut(data_len);
            remaining = rest;
            accounts.push(Account::new(resource_id, is_new, slice));
        }

        TransactionContext {
            metadata: Metadata { tx_index, tx_bytes, block_hash: block_hash_bytes, blue_score },
            accounts,
        }
    }
}

#[cfg(feature = "host")]
impl TransactionContext<'_> {
    /// Encodes a scheduler [`TransactionContext`](vprogs_scheduling_scheduler::TransactionContext)
    /// into the ABI wire format.
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

        let fixed_header = 4 + 4 + 32 + 8 + 4;
        let accounts_header = resources.len() * ACCOUNT_HEADER_SIZE;
        let payload_len: usize = resources.iter().map(|r| r.data().len()).sum();
        let total = fixed_header + tx_bytes.len() + accounts_header + payload_len;

        let mut buf = Vec::with_capacity(total);

        // Fixed header
        buf.extend_from_slice(&ctx.tx_index().to_le_bytes());
        buf.extend_from_slice(&(resources.len() as u32).to_le_bytes());
        buf.extend_from_slice(&chain_metadata.block_hash().as_bytes());
        buf.extend_from_slice(&chain_metadata.blue_score().to_le_bytes());
        buf.extend_from_slice(&(tx_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&tx_bytes);

        // Per-account headers
        for r in resources {
            buf.extend_from_slice(r.access_metadata().resource_id.as_bytes());
            let flags = if r.is_new() { 1u8 } else { 0u8 };
            buf.push(flags);
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

#[cfg(test)]
fn encode_test(
    tx_index: u32,
    block_hash: [u8; 32],
    blue_score: u64,
    tx_bytes: &[u8],
    accounts: &[(&[u8; 32], bool, &[u8])],
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&tx_index.to_le_bytes());
    buf.extend_from_slice(&(accounts.len() as u32).to_le_bytes());
    buf.extend_from_slice(&block_hash);
    buf.extend_from_slice(&blue_score.to_le_bytes());
    buf.extend_from_slice(&(tx_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(tx_bytes);
    for &(rid, is_new, data) in accounts {
        buf.extend_from_slice(rid);
        buf.push(if is_new { 1 } else { 0 });
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    }
    for &(_, _, data) in accounts {
        buf.extend_from_slice(data);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let rid1 = [1u8; 32];
        let rid2 = [2u8; 32];
        let mut blob = encode_test(
            42,
            [7u8; 32],
            12345,
            &[0xAA, 0xBB, 0xCC],
            &[(&rid1, true, &[10, 20, 30]), (&rid2, false, &[40, 50])],
        );
        let ctx = TransactionContext::decode(&mut blob);

        assert_eq!(ctx.metadata.tx_index, 42);
        assert_eq!(ctx.metadata.tx_bytes, &[0xAA, 0xBB, 0xCC]);
        assert_eq!(ctx.metadata.block_hash, [7u8; 32]);
        assert_eq!(ctx.metadata.blue_score, 12345);

        assert_eq!(ctx.accounts.len(), 2);
        assert_eq!(*ctx.accounts[0].resource_id(), ResourceId::from([1u8; 32]));
        assert!(ctx.accounts[0].is_new());
        assert_eq!(ctx.accounts[0].data(), &[10, 20, 30]);
        assert_eq!(*ctx.accounts[1].resource_id(), ResourceId::from([2u8; 32]));
        assert!(!ctx.accounts[1].is_new());
        assert_eq!(ctx.accounts[1].data(), &[40, 50]);
    }

    #[test]
    fn encode_decode_empty_accounts() {
        let mut blob = encode_test(0, [0u8; 32], 0, &[], &[]);
        let ctx = TransactionContext::decode(&mut blob);

        assert_eq!(ctx.accounts.len(), 0);
        assert_eq!(ctx.metadata.tx_bytes, &[] as &[u8]);
    }
}
