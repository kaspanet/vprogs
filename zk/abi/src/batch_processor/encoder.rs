use alloc::vec::Vec;

use super::{HEADER_SIZE, RESOURCE_COMMITMENT_SIZE};

/// Encodes a batch witness into bytes (host-side).
pub fn encode_batch_witness(
    image_id: &[u8; 32],
    batch_index: u64,
    prev_root: &[u8; 32],
    commitments: &[([u8; 32], [u8; 32])], // (resource_id, leaf_hash)
    multi_proof_bytes: &[u8],
    txs: &[(Vec<u8>, Vec<u8>, Vec<u8>)], // (journal, wire_bytes, exec_result)
) -> Vec<u8> {
    let n_resources = commitments.len() as u32;
    let n_txs = txs.len() as u32;

    let tx_payload_size: usize =
        txs.iter().map(|(j, w, e)| 4 + j.len() + 4 + w.len() + 4 + e.len()).sum();

    let total = HEADER_SIZE
        + (n_resources as usize) * RESOURCE_COMMITMENT_SIZE
        + 4 + multi_proof_bytes.len() // length prefix + raw bytes
        + tx_payload_size;

    let mut buf = Vec::with_capacity(total);

    // Header
    buf.extend_from_slice(image_id);
    buf.extend_from_slice(&batch_index.to_le_bytes());
    buf.extend_from_slice(prev_root);
    buf.extend_from_slice(&n_resources.to_le_bytes());
    buf.extend_from_slice(&n_txs.to_le_bytes());

    // Resource commitments
    for (resource_id, leaf_hash) in commitments {
        buf.extend_from_slice(resource_id);
        buf.extend_from_slice(leaf_hash);
    }

    // Multi-proof (length-prefixed raw bytes)
    buf.extend_from_slice(&(multi_proof_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(multi_proof_bytes);

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
