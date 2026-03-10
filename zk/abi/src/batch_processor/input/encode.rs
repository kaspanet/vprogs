use alloc::vec::Vec;

use super::{HEADER_SIZE, RESOURCE_COMMITMENT_SIZE};

/// Encodes the batch processor input into bytes (host-side).
pub fn encode(
    image_id: &[u8; 32],
    batch_index: u64,
    prev_root: &[u8; 32],
    commitments: &[([u8; 32], [u8; 32])], // (resource_id, leaf_hash)
    multi_proof_bytes: &[u8],
    journals: &[Vec<u8>],
) -> Vec<u8> {
    let n_resources = commitments.len() as u32;
    let n_txs = journals.len() as u32;

    let tx_payload_size: usize = journals.iter().map(|j| 4 + j.len()).sum();

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

    // Transaction entries (journal only)
    for journal in journals {
        buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
        buf.extend_from_slice(journal);
    }

    debug_assert_eq!(buf.len(), total);
    buf
}
