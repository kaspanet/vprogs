use alloc::vec::Vec;

use vprogs_core_codec::Reader;
use vprogs_core_smt::proving::Proof;

use crate::{Result, batch_processor::TransactionJournals};

/// Decoded batch processor input (zero-copy).
///
/// Wire layout:
///
/// ```text
/// image_id(32) | covenant_id(32) | prev_seq(32)
///   | parent_seq_commit(32)
///   | blue_score(8) | daa_score(8) | parent_timestamp(8)
///   | prev_lane_tip(32) | lane_blue_score(8) | lane_expired(1)
///   | lane_key(32)
///   | miner_payload_leaves_count(u32 LE) | miner_payload_leaves (32-byte hashes)
///   | lane_smt_proof_len(u32 LE) | lane_smt_proof bytes
///   | smt_proof (length-prefixed) | leaf_order | tx_journals
/// ```
///
/// `new_seq` is no longer an input — the guest derives it from these ingredients via kip21
/// primitives and commits the result in the 160-byte settlement journal.
pub struct Inputs<'a> {
    /// Transaction processor guest image ID used to verify each inner tx journal.
    pub image_id: &'a [u8; 32],
    /// Covenant id the emitted settlement journal binds to.
    pub covenant_id: &'a [u8; 32],
    /// Sequencing commitment entering the batch (covenant redeem prefix enforces this).
    pub prev_seq: &'a [u8; 32],
    /// `seq_commit` of this block's immediate DAG selected parent — the `H_seq` chain input
    /// for kip21's `seq_commit = H_seq(parent_seq_commit, state_root)` computation.
    pub parent_seq_commit: &'a [u8; 32],
    /// DAG blue score of this chain block.
    pub blue_score: u64,
    /// DAA score of this chain block.
    pub daa_score: u64,
    /// Selected-parent timestamp (used by `mergeset_context_hash`'s `seq_commit_timestamp`).
    pub parent_timestamp: u64,
    /// Our lane's tip entering this block (pre-update).
    pub prev_lane_tip: &'a [u8; 32],
    /// Blue score at which our lane was last active.
    pub lane_blue_score: u64,
    /// True when the lane was silent past the finality window and re-anchors on
    /// `parent_seq_commit` instead of `prev_lane_tip`.
    pub lane_expired: bool,
    /// Our lane key.
    pub lane_key: &'a [u8; 32],
    /// `miner_payload_leaf(...)` hashes over this block's mergeset.
    pub miner_payload_leaves: Vec<&'a [u8; 32]>,
    /// Serialized `kaspa_smt::proof::OwnedSmtProof` for `lane_key` against this block's
    /// post-update `lanes_root`.
    pub lane_smt_proof: &'a [u8],
    /// Sparse Merkle tree proof over L2 state (leaves carry pre-batch key + value_hash per
    /// resource).
    pub proof: Proof<'a>,
    /// Leaf order mapping: `leaf_order[leaf_pos] = resource_index`.
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
        let parent_seq_commit = buf.array::<32>("parent_seq_commit")?;
        let blue_score = buf.le_u64("blue_score")?;
        let daa_score = buf.le_u64("daa_score")?;
        let parent_timestamp = buf.le_u64("parent_timestamp")?;
        let prev_lane_tip = buf.array::<32>("prev_lane_tip")?;
        let lane_blue_score = buf.le_u64("lane_blue_score")?;
        let lane_expired = buf.byte("lane_expired")? != 0;
        let lane_key = buf.array::<32>("lane_key")?;

        let miner_payload_count = buf.le_u32("miner_payload_count")? as usize;
        let mut miner_payload_leaves = Vec::with_capacity(miner_payload_count);
        for _ in 0..miner_payload_count {
            miner_payload_leaves.push(buf.array::<32>("miner_payload_leaf")?);
        }

        let lane_smt_proof_len = buf.le_u32("lane_smt_proof_len")? as usize;
        let lane_smt_proof = buf.bytes(lane_smt_proof_len, "lane_smt_proof")?;

        let proof_length = buf.le_u32("proof_length")? as usize;
        let proof = Proof::decode(buf.bytes(proof_length, "proof")?)?;

        let n_resources = proof.leaves.len();
        let mut leaf_order = Vec::with_capacity(n_resources);
        for _ in 0..n_resources {
            leaf_order.push(buf.le_u32("leaf_order")?);
        }

        let tx_journals = TransactionJournals::new(buf);

        Ok(Self {
            image_id,
            covenant_id,
            prev_seq,
            parent_seq_commit,
            blue_score,
            daa_score,
            parent_timestamp,
            prev_lane_tip,
            lane_blue_score,
            lane_expired,
            lane_key,
            miner_payload_leaves,
            lane_smt_proof,
            proof,
            leaf_order,
            tx_journals,
        })
    }

    /// Encodes the batch processor input into bytes (host-side).
    #[cfg(feature = "host")]
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        image_id: &[u8; 32],
        covenant_id: &[u8; 32],
        prev_seq: &[u8; 32],
        parent_seq_commit: &[u8; 32],
        blue_score: u64,
        daa_score: u64,
        parent_timestamp: u64,
        prev_lane_tip: &[u8; 32],
        lane_blue_score: u64,
        lane_expired: bool,
        lane_key: &[u8; 32],
        miner_payload_leaves: &[[u8; 32]],
        lane_smt_proof: &[u8],
        proof_bytes: &[u8],
        leaf_order: &[u32],
        tx_journals: &[Vec<u8>],
    ) -> Vec<u8> {
        use crate::Write;

        let journals_size: usize = tx_journals.iter().map(|j| 4 + j.len()).sum();
        let total = 32 * 5                                    // image_id, covenant_id, prev_seq, parent_seq_commit, prev_lane_tip, lane_key
            + 32                                              // lane_key (counted above as 5; one more 32 for parent_seq_commit → recount below)
            + 8 * 4                                           // blue_score, daa_score, parent_timestamp, lane_blue_score
            + 1                                               // lane_expired
            + 4 + 32 * miner_payload_leaves.len()            // miner_payload_leaves
            + 4 + lane_smt_proof.len()                       // lane_smt_proof
            + 4 + proof_bytes.len()                          // smt proof
            + leaf_order.len() * 4                           // leaf_order
            + journals_size;
        let _ = total; // capacity hint is approximate; Vec grows as needed.
        let mut buf = Vec::new();

        buf.write(image_id);
        buf.write(covenant_id);
        buf.write(prev_seq);
        buf.write(parent_seq_commit);
        buf.extend_from_slice(&blue_score.to_le_bytes());
        buf.extend_from_slice(&daa_score.to_le_bytes());
        buf.extend_from_slice(&parent_timestamp.to_le_bytes());
        buf.write(prev_lane_tip);
        buf.extend_from_slice(&lane_blue_score.to_le_bytes());
        buf.push(if lane_expired { 1 } else { 0 });
        buf.write(lane_key);

        buf.extend_from_slice(&(miner_payload_leaves.len() as u32).to_le_bytes());
        for leaf in miner_payload_leaves {
            buf.write(leaf);
        }

        buf.extend_from_slice(&(lane_smt_proof.len() as u32).to_le_bytes());
        buf.extend_from_slice(lane_smt_proof);

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
