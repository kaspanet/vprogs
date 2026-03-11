use alloc::vec::Vec;

use vprogs_zk_smt::MultiProof;

use super::{RESOURCE_COMMITMENT_SIZE, header::Header, journal_iter::JournalIter};
use crate::transaction_processor::InputResourceCommitment;

/// Decodes the batch processor input from a raw byte buffer into zero-copy views.
///
/// Returns the header, resource commitments, multi-proof, and a transaction entry iterator,
/// all borrowing from `buf`.
///
/// # Panics
/// Panics if the buffer is truncated or malformed.
pub fn decode(
    buf: &[u8],
) -> (Header<'_>, Vec<InputResourceCommitment<'_>>, MultiProof<'_>, JournalIter<'_>) {
    let header = Header::decode(buf);

    let commitments_end = Header::SIZE + (header.n_resources as usize) * RESOURCE_COMMITMENT_SIZE;
    assert!(buf.len() >= commitments_end, "input too short for resource commitments");

    let commitments = (0..header.n_resources)
        .map(|i| {
            let base = Header::SIZE + (i as usize) * RESOURCE_COMMITMENT_SIZE;
            InputResourceCommitment::decode_pre_indexed(&buf[base..], i)
        })
        .collect();

    // Read multi-proof length prefix.
    let mp_len = u32::from_le_bytes(
        buf[commitments_end..commitments_end + 4].try_into().expect("truncated multi-proof length"),
    ) as usize;
    let mp_start = commitments_end + 4;
    let multi_proof = MultiProof::decode(&buf[mp_start..mp_start + mp_len]);

    let tx_offset = mp_start + mp_len;
    let tx_entries = JournalIter { buf, offset: tx_offset, remaining: header.n_txs };

    (header, commitments, multi_proof, tx_entries)
}
