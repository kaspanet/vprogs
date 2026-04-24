use alloc::vec::Vec;

use vprogs_core_codec::Reader;
use vprogs_core_smt::proving::Proof;

use crate::{Result, batch_processor::TransactionJournals};

/// Decoded batch processor input (zero-copy).
///
/// Wire layout:
/// `image_id(32) | covenant_id(32) | prev_seq(32) | new_seq(32) | proof (length-prefixed) |
/// leaf_order | tx_journals`
pub struct Inputs<'a> {
    /// Transaction processor guest image ID used to verify each inner tx journal.
    pub image_id: &'a [u8; 32],
    /// Covenant id the emitted settlement journal binds to.
    pub covenant_id: &'a [u8; 32],
    /// Sequencing commitment entering the batch (previous covenant UTXO's anchor).
    pub prev_seq: &'a [u8; 32],
    /// Sequencing commitment after the batch (what the covenant will cross-check against
    /// `OpChainblockSeqCommit(block_prove_to)`).
    pub new_seq: &'a [u8; 32],
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
        let image_id = buf.array::<32>("image_id")?;
        let covenant_id = buf.array::<32>("covenant_id")?;
        let prev_seq = buf.array::<32>("prev_seq")?;
        let new_seq = buf.array::<32>("new_seq")?;

        let proof_length = buf.le_u32("proof_length")? as usize;
        let proof = Proof::decode(buf.bytes(proof_length, "proof")?)?;

        let n_resources = proof.leaves.len();
        let mut leaf_order = Vec::with_capacity(n_resources);
        for _ in 0..n_resources {
            leaf_order.push(buf.le_u32("leaf_order")?);
        }

        let tx_journals = TransactionJournals::new(buf);

        Ok(Self { image_id, covenant_id, prev_seq, new_seq, proof, leaf_order, tx_journals })
    }

    /// Encodes the batch processor input into bytes (host-side).
    #[cfg(feature = "host")]
    pub fn encode(
        image_id: &[u8; 32],
        covenant_id: &[u8; 32],
        prev_seq: &[u8; 32],
        new_seq: &[u8; 32],
        proof_bytes: &[u8],
        leaf_order: &[u32],
        tx_journals: &[Vec<u8>],
    ) -> Vec<u8> {
        use crate::Write;

        let journals_size: usize = tx_journals.iter().map(|j| 4 + j.len()).sum();
        let total =
            32 + 32 + 32 + 32 + 4 + proof_bytes.len() + leaf_order.len() * 4 + journals_size;
        let mut buf = Vec::with_capacity(total);

        buf.write(image_id);
        buf.write(covenant_id);
        buf.write(prev_seq);
        buf.write(new_seq);

        buf.extend_from_slice(&(proof_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(proof_bytes);

        for &idx in leaf_order {
            buf.extend_from_slice(&idx.to_le_bytes());
        }

        for journal in tx_journals {
            buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
            buf.extend_from_slice(journal);
        }

        buf
    }
}
