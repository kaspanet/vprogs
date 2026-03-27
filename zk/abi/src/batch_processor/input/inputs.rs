use alloc::vec::Vec;

use vprogs_core_codec::Reader;
use vprogs_core_smt::proving::Proof;

use crate::{Result, batch_processor::TransactionJournals};

/// Decoded batch processor input (zero-copy).
///
/// Wire layout: `image_id(32) | proof (length-prefixed) | leaf_order | tx_journals`
pub struct Inputs<'a> {
    /// Transaction processor guest image ID.
    pub image_id: &'a [u8; 32],
    /// Sparse Merkle tree proof (leaves carry pre-batch key + value_hash per resource).
    pub proof: Proof<'a>,
    /// Leaf order mapping: `leaf_order[leaf_pos] = resource_index` (materialized for O(1) access).
    pub leaf_order: Vec<u32>,
    /// Iterator over per-transaction journal entries.
    pub tx_journals: TransactionJournals<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the batch processor input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        // Image ID (32 bytes).
        let image_id = buf.array::<32>("image_id")?;

        // Decode length-prefixed proof (comes before leaf_order so we know the leaf count).
        let proof_length = buf.le_u32("proof_length")? as usize;
        let proof = Proof::decode(buf.bytes(proof_length, "proof")?)?;

        // Decode leaf_order (one u32 per leaf, count derived from proof).
        let n_resources = proof.leaves.len();
        let mut leaf_order = Vec::with_capacity(n_resources);
        for _ in 0..n_resources {
            leaf_order.push(buf.le_u32("leaf_order")?);
        }

        // Remaining bytes are per-transaction journal entries.
        let tx_journals = TransactionJournals::new(buf);

        Ok(Self { image_id, proof, leaf_order, tx_journals })
    }

    /// Encodes the batch processor input into bytes (host-side).
    ///
    /// Wire layout: `image_id(32) | proof (length-prefixed) | leaf_order | tx_journals`
    #[cfg(feature = "host")]
    pub fn encode(
        image_id: &[u8; 32],
        proof_bytes: &[u8],
        leaf_order: &[u32],
        tx_journals: &[Vec<u8>],
    ) -> Vec<u8> {
        use crate::Write;

        let journals_size: usize = tx_journals.iter().map(|j| 4 + j.len()).sum();
        let total = 32 + 4 + proof_bytes.len() + leaf_order.len() * 4 + journals_size;
        let mut buf = Vec::with_capacity(total);

        // Image ID.
        buf.write(image_id);

        // Proof (length-prefixed raw bytes).
        buf.extend_from_slice(&(proof_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(proof_bytes);

        // Leaf order mapping (one u32 per leaf).
        for &idx in leaf_order {
            buf.extend_from_slice(&idx.to_le_bytes());
        }

        // Transaction journals (each length-prefixed).
        for journal in tx_journals {
            buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
            buf.extend_from_slice(journal);
        }

        buf
    }
}
