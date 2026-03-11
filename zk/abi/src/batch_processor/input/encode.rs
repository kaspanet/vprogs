use alloc::vec::Vec;

use super::header::Header;
use crate::transaction_processor::InputResourceCommitment;

/// Encodes the batch processor input into bytes (host-side).
pub fn encode(
    header: &Header<'_>,
    commitments: &[InputResourceCommitment<'_>],
    multi_proof_bytes: &[u8],
    journals: &[Vec<u8>],
) -> Vec<u8> {
    let tx_payload_size: usize = journals.iter().map(|j| 4 + j.len()).sum();

    let total = Header::SIZE
        + header.n_resources as usize * InputResourceCommitment::PRE_INDEXED_SIZE
        + 4 + multi_proof_bytes.len() // length prefix + raw bytes
        + tx_payload_size;

    let mut buf = Vec::with_capacity(total);

    header.encode(&mut buf);

    // Resource commitments (pre-indexed: resource_id + hash, no index)
    for c in commitments {
        buf.extend_from_slice(c.resource_id);
        buf.extend_from_slice(c.hash);
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
