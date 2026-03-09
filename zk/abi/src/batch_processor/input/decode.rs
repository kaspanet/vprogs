use alloc::vec::Vec;

use vprogs_zk_smt::MultiProof;

use super::{
    HEADER_SIZE, RESOURCE_COMMITMENT_SIZE, header::Header, resource_commitment::ResourceCommitment,
    tx_entry::TxEntryIter,
};

/// Decodes the batch processor input from a raw byte buffer into zero-copy views.
///
/// Returns the header, resource commitments, multi-proof, and a transaction entry iterator,
/// all borrowing from `buf`.
///
/// # Panics
/// Panics if the buffer is truncated or malformed.
pub fn decode(
    buf: &[u8],
) -> (Header<'_>, Vec<ResourceCommitment<'_>>, MultiProof<'_>, TxEntryIter<'_>) {
    assert!(buf.len() >= HEADER_SIZE, "input too short for header");

    let n_resources = u32::from_le_bytes(buf[72..76].try_into().expect("truncated n_resources"));
    let n_txs = u32::from_le_bytes(buf[76..80].try_into().expect("truncated n_txs"));

    let commitments_end = HEADER_SIZE + (n_resources as usize) * RESOURCE_COMMITMENT_SIZE;
    assert!(buf.len() >= commitments_end, "input too short for resource commitments");

    let header = Header {
        image_id: buf[0..32].try_into().expect("truncated image_id"),
        batch_index: u64::from_le_bytes(buf[32..40].try_into().expect("truncated batch_index")),
        prev_root: buf[40..72].try_into().expect("truncated prev_root"),
        n_resources,
        n_txs,
    };

    let commitments = (0..n_resources)
        .map(|i| {
            let base = HEADER_SIZE + (i as usize) * RESOURCE_COMMITMENT_SIZE;
            ResourceCommitment {
                resource_id: buf[base..base + 32].try_into().expect("truncated resource_id"),
                hash: buf[base + 32..base + 64].try_into().expect("truncated hash"),
            }
        })
        .collect();

    // Read multi-proof length prefix.
    let mp_len = u32::from_le_bytes(
        buf[commitments_end..commitments_end + 4].try_into().expect("truncated multi-proof length"),
    ) as usize;
    let mp_start = commitments_end + 4;
    let multi_proof = MultiProof::decode(&buf[mp_start..mp_start + mp_len]);

    let tx_offset = mp_start + mp_len;
    let tx_entries = TxEntryIter { buf, offset: tx_offset, remaining: n_txs };

    (header, commitments, multi_proof, tx_entries)
}
