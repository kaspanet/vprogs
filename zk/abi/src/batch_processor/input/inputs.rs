use alloc::vec::Vec;

use vprogs_core_codec::Reader;
use vprogs_core_smt::proving::Proof;

use super::{header::Header, journal_iter::JournalIter};
use crate::Result;

/// Decoded batch processor input (zero-copy).
pub struct Inputs<'a> {
    /// Batch header.
    pub header: Header<'a>,
    /// Leaf order mapping: `leaf_order[leaf_pos] = resource_index`. Maps each proof leaf back
    /// to its position in the scheduler's resource ordering.
    pub leaf_order: Vec<u32>,
    /// Sparse Merkle tree proof (leaves carry pre-batch key + value_hash per resource).
    pub proof: Proof<'a>,
    /// Iterator over per-transaction journal entries.
    pub tx_journals: JournalIter<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the batch processor input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        // Decode header.
        let header = Header::decode(&mut buf)?;

        // Decode leaf_order (one u32 per leaf).
        let mut leaf_order = Vec::with_capacity(header.n_resources as usize);
        for _ in 0..header.n_resources {
            leaf_order.push(buf.le_u32("leaf_order")?);
        }

        // Decode length-prefixed proof.
        let proof_length = buf.le_u32("proof_length")? as usize;
        let proof = Proof::decode(buf.bytes(proof_length, "proof")?)?;

        // Remaining bytes are per-transaction journal entries.
        let tx_journals = JournalIter::new(buf, header.n_txs);

        Ok(Self { header, leaf_order, proof, tx_journals })
    }

    /// Encodes the batch processor input into bytes (host-side).
    #[cfg(feature = "host")]
    pub fn encode(
        header: &Header<'_>,
        leaf_order: &[u32],
        proof_bytes: &[u8],
        tx_journals: &[Vec<u8>],
    ) -> Vec<u8> {
        let journals_size: usize = tx_journals.iter().map(|j| 4 + j.len()).sum();

        let total = Header::SIZE + leaf_order.len() * 4 + 4 + proof_bytes.len() + journals_size;

        let mut buf = Vec::with_capacity(total);

        header.encode(&mut buf);

        // Leaf order mapping (one u32 per leaf).
        for &idx in leaf_order {
            buf.extend_from_slice(&idx.to_le_bytes());
        }

        // Proof (length-prefixed raw bytes).
        buf.extend_from_slice(&(proof_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(proof_bytes);

        // Transaction entries (journal only).
        for journal in tx_journals {
            buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
            buf.extend_from_slice(journal);
        }

        debug_assert_eq!(buf.len(), total);
        buf
    }
}
