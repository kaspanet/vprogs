use alloc::vec::Vec;

use vprogs_core_codec::Reader;
use vprogs_zk_smt::MultiProof;

use super::{header::Header, journal_iter::JournalIter};
use crate::{Result, transaction_processor::InputResourceCommitment};

/// Decoded batch processor input (zero-copy).
pub struct Inputs<'a> {
    /// Batch header.
    pub header: Header<'a>,
    /// Per-resource input commitments.
    pub commitments: Vec<InputResourceCommitment<'a>>,
    /// Sparse Merkle tree multi-proof.
    pub multi_proof: MultiProof<'a>,
    /// Iterator over per-transaction journal entries.
    pub tx_entries: JournalIter<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the batch processor input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        // Decode header.
        let header = Header::decode(&mut buf)?;

        // Decode per-resource input commitments.
        let n = header
            .n_resources
            .checked_mul(InputResourceCommitment::PRE_INDEXED_SIZE as u32)
            .filter(|&total| (total as usize) <= buf.len())
            .ok_or_else(|| crate::Error::Decode("n_resources overflow".into()))?;
        let _ = n; // validation only; we still decode one-by-one for the index assignment

        let mut commitments = Vec::with_capacity(header.n_resources as usize);
        for i in 0..header.n_resources {
            commitments.push(InputResourceCommitment::decode_pre_indexed(&mut buf, i)?);
        }

        // Decode length-prefixed multi-proof.
        let multi_proof_length = buf.le_u32("multi_proof_length")? as usize;
        let multi_proof = MultiProof::decode(buf.bytes(multi_proof_length, "multi_proof")?);

        // Remaining bytes are per-transaction journal entries.
        let tx_entries = JournalIter::new(buf, header.n_txs);

        Ok(Self { header, commitments, multi_proof, tx_entries })
    }

    /// Encodes the batch processor input into bytes (host-side).
    #[cfg(feature = "host")]
    pub fn encode(
        header: &Header<'_>,
        commitments: &[InputResourceCommitment<'_>],
        multi_proof_bytes: &[u8],
        journals: &[Vec<u8>],
    ) -> Vec<u8> {
        let tx_payload_size: usize = journals.iter().map(|j| 4 + j.len()).sum();

        let total = Header::SIZE
            + header.n_resources as usize * InputResourceCommitment::PRE_INDEXED_SIZE
            + 4
            + multi_proof_bytes.len()
            + tx_payload_size;

        let mut buf = Vec::with_capacity(total);

        header.encode(&mut buf);

        // Resource commitments (pre-indexed: resource_id + hash, no index).
        for c in commitments {
            c.encode_pre_indexed(&mut buf);
        }

        // Multi-proof (length-prefixed raw bytes).
        buf.extend_from_slice(&(multi_proof_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(multi_proof_bytes);

        // Transaction entries (journal only).
        for journal in journals {
            buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
            buf.extend_from_slice(journal);
        }

        debug_assert_eq!(buf.len(), total);
        buf
    }
}
