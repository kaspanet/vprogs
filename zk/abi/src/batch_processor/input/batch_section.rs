use alloc::vec::Vec;

use vprogs_core_codec::{Reader, Result};

/// One batch's portion of a bundle: the per-chain-block context and the txs that landed on
/// our lane in that block.
///
/// A bundle proof carries K of these in scheduling order. Per-section state in the batch
/// guest (activity digest, expected metadata, last_tx_index, derived context_hash) resets at
/// each section boundary; bundle-wide state (`value_hashes`, `current_lane_tip`) carries
/// forward.
pub struct BatchSection<'a> {
    /// DAG blue score of this section's chain block.
    pub blue_score: u64,
    /// DAA score of this section's chain block.
    pub daa_score: u64,
    /// Selected-parent timestamp (used by `mergeset_context_hash`'s `seq_commit_timestamp`).
    pub parent_timestamp: u64,
    /// Lane tip entering this section's block.
    pub prev_lane_tip: &'a [u8; 32],
    /// Blue score at which the lane was last active before this block.
    pub lane_blue_score: u64,
    /// True when the lane was silent past the finality window and re-anchors on
    /// `parent_seq_commit` instead of `prev_lane_tip`.
    pub lane_expired: bool,
    /// `seq_commit` of this block's selected parent (used iff `lane_expired`).
    pub parent_seq_commit: &'a [u8; 32],
    /// Translation from this batch's batch-local `resource_index` to the bundle-wide
    /// resource_index space (= position into `Inputs::leaf_order` / `value_hashes`).
    pub batch_to_bundle_index: Vec<u32>,
    /// Wire bytes for this section's per-tx journal entries. Construct a
    /// [`TransactionJournals`] over this slice when iterating.
    pub tx_journals_buf: &'a [u8],
}

impl<'a> BatchSection<'a> {
    /// Decodes one `BatchSection` from a wire buffer (zero-copy, advances `buf`).
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            blue_score: buf.le_u64("blue_score")?,
            daa_score: buf.le_u64("daa_score")?,
            parent_timestamp: buf.le_u64("parent_timestamp")?,
            prev_lane_tip: buf.array::<32>("prev_lane_tip")?,
            lane_blue_score: buf.le_u64("lane_blue_score")?,
            lane_expired: buf.byte("lane_expired")? != 0,
            parent_seq_commit: buf.array::<32>("parent_seq_commit")?,
            batch_to_bundle_index: buf
                .many("batch_to_bundle", |b| b.le_u32("batch_to_bundle_index"))?,
            tx_journals_buf: buf.blob("tx_journals")?,
        })
    }

    /// Encodes one section to bytes (host-side).
    #[cfg(feature = "host")]
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        buf: &mut Vec<u8>,
        blue_score: u64,
        daa_score: u64,
        parent_timestamp: u64,
        prev_lane_tip: &[u8; 32],
        lane_blue_score: u64,
        lane_expired: bool,
        parent_seq_commit: &[u8; 32],
        batch_to_bundle_index: &[u32],
        tx_journals: &[Vec<u8>],
    ) {
        buf.extend_from_slice(&blue_score.to_le_bytes());
        buf.extend_from_slice(&daa_score.to_le_bytes());
        buf.extend_from_slice(&parent_timestamp.to_le_bytes());
        buf.extend_from_slice(prev_lane_tip);
        buf.extend_from_slice(&lane_blue_score.to_le_bytes());
        buf.push(if lane_expired { 1 } else { 0 });
        buf.extend_from_slice(parent_seq_commit);

        buf.extend_from_slice(&(batch_to_bundle_index.len() as u32).to_le_bytes());
        for &idx in batch_to_bundle_index {
            buf.extend_from_slice(&idx.to_le_bytes());
        }

        // Section-local tx_journals carry their own length so we know where the section ends.
        let mut tx_journals_buf: Vec<u8> = Vec::new();
        for journal in tx_journals {
            tx_journals_buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
            tx_journals_buf.extend_from_slice(journal);
        }
        buf.extend_from_slice(&(tx_journals_buf.len() as u32).to_le_bytes());
        buf.extend_from_slice(&tx_journals_buf);
    }
}
