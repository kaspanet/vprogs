use alloc::vec::Vec;

use vprogs_zk_smt::MultiProof;

use super::{header::Header, journal_iter::JournalIter};
use crate::{Parser, Result, transaction_processor::InputResourceCommitment};

/// Decodes the batch processor input from a raw byte buffer into zero-copy views.
///
/// Returns the header, resource commitments, multi-proof, and a transaction entry iterator,
/// all borrowing from `buf`.
pub fn decode(
    mut buf: &[u8],
) -> Result<(Header<'_>, Vec<InputResourceCommitment<'_>>, MultiProof<'_>, JournalIter<'_>)> {
    // Decode header.
    let header = Header::decode(&mut buf)?;

    // Decode per-resource input commitments.
    let mut commitments = Vec::with_capacity(header.n_resources as usize);
    for i in 0..header.n_resources {
        commitments.push(InputResourceCommitment::decode_pre_indexed(&mut buf, i)?);
    }

    // Decode length-prefixed multi-proof.
    let multi_proof_length = buf.consume_u32("multi_proof_length")? as usize;
    let multi_proof = MultiProof::decode(buf.consume_bytes(multi_proof_length, "multi_proof")?);

    // Remaining bytes are per-transaction journal entries.
    let tx_entries = JournalIter::new(buf, header.n_txs);

    Ok((header, commitments, multi_proof, tx_entries))
}
