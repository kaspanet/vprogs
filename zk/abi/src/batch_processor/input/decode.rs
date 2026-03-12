use alloc::vec::Vec;

use vprogs_zk_smt::MultiProof;

use super::{header::Header, journal_iter::JournalIter};
use crate::{Parser, Result, transaction_processor::InputResourceCommitment};

/// Decodes the batch processor input from a raw byte buffer into zero-copy views.
///
/// Returns the header, resource commitments, multi-proof, and a transaction entry iterator,
/// all borrowing from `buf`.
pub fn decode(
    buf: &[u8],
) -> Result<(Header<'_>, Vec<InputResourceCommitment<'_>>, MultiProof<'_>, JournalIter<'_>)> {
    let header = Header::decode(buf)?;

    let commitments_end =
        Header::SIZE + (header.n_resources as usize) * InputResourceCommitment::PRE_INDEXED_SIZE;

    let mut commitments_buf = &buf[Header::SIZE..commitments_end];
    let mut commitments = Vec::with_capacity(header.n_resources as usize);
    for i in 0..header.n_resources {
        commitments.push(InputResourceCommitment::decode_pre_indexed(&mut commitments_buf, i)?);
    }

    // Read multi-proof length prefix.
    let multi_proof_length =
        buf[commitments_end..commitments_end + 4].parse_u32("multi_proof_length")? as usize;
    let mp_start = commitments_end + 4;
    let multi_proof = MultiProof::decode(&buf[mp_start..mp_start + multi_proof_length]);

    let tx_entries = JournalIter::new(&buf[mp_start + multi_proof_length..], header.n_txs);

    Ok((header, commitments, multi_proof, tx_entries))
}
