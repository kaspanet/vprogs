use alloc::vec::Vec;

use vprogs_zk_smt::MultiProof;

use super::{ACCOUNT_ENTRY_SIZE, HEADER_SIZE};

/// Encodes a batch witness into bytes (host-side).
pub fn encode_batch_witness(
    image_id: &[u8; 32],
    batch_index: u64,
    prev_root: &[u8; 32],
    accounts: &[([u8; 32], bool, [u8; 32])], // (resource_id, is_new, leaf_hash)
    multi_proof: &MultiProof,
    txs: &[(Vec<u8>, Vec<u8>, Vec<u8>)], // (journal, wire_bytes, exec_result)
) -> Vec<u8> {
    let multi_proof_bytes = borsh::to_vec(multi_proof).expect("failed to serialize multi-proof");

    let n_accounts = accounts.len() as u32;
    let n_txs = txs.len() as u32;

    let tx_payload_size: usize =
        txs.iter().map(|(j, w, e)| 4 + j.len() + 4 + w.len() + 4 + e.len()).sum();

    let total = HEADER_SIZE
        + (n_accounts as usize) * ACCOUNT_ENTRY_SIZE
        + 4 + multi_proof_bytes.len() // length prefix + borsh data
        + tx_payload_size;

    let mut buf = Vec::with_capacity(total);

    // Header
    buf.extend_from_slice(image_id);
    buf.extend_from_slice(&batch_index.to_le_bytes());
    buf.extend_from_slice(prev_root);
    buf.extend_from_slice(&n_accounts.to_le_bytes());
    buf.extend_from_slice(&n_txs.to_le_bytes());

    // Account entries
    for (resource_id, is_new, leaf_hash) in accounts {
        buf.extend_from_slice(resource_id);
        buf.push(if *is_new { 1 } else { 0 });
        buf.extend_from_slice(leaf_hash);
    }

    // Multi-proof (length-prefixed borsh)
    buf.extend_from_slice(&(multi_proof_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&multi_proof_bytes);

    // Transaction entries
    for (journal, wire_bytes, exec_result) in txs {
        buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
        buf.extend_from_slice(journal);
        buf.extend_from_slice(&(wire_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(wire_bytes);
        buf.extend_from_slice(&(exec_result.len() as u32).to_le_bytes());
        buf.extend_from_slice(exec_result);
    }

    debug_assert_eq!(buf.len(), total);
    buf
}
