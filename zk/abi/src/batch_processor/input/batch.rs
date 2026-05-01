use alloc::vec::Vec;

use vprogs_core_codec::{Reader, Result};

#[cfg(feature = "host")]
use crate::Write;
#[cfg(feature = "host")]
use crate::batch_processor::BundlePart;
use crate::batch_processor::TransactionJournals;

/// One batch of a bundle: per-block context plus the lane txs in that block.
pub struct Batch<'a> {
    /// DAG blue score of this batch's chain block.
    pub blue_score: u64,
    /// DAA score of this batch's chain block.
    pub daa_score: u64,
    /// Selected-parent timestamp.
    pub parent_timestamp: u64,
    /// Lane tip entering this batch's block.
    pub prev_lane_tip: &'a [u8; 32],
    /// Blue score at which the lane was last active before this block.
    pub lane_blue_score: u64,
    /// True when the lane re-anchors on `parent_seq_commit` instead of `prev_lane_tip`.
    pub lane_expired: bool,
    /// `seq_commit` of this block's selected parent (used iff `lane_expired`).
    pub parent_seq_commit: &'a [u8; 32],
    /// Maps batch-local `resource_index` to bundle-wide resource_index.
    pub translation: Vec<u32>,
    /// Per-tx journal entries.
    pub tx_journals: TransactionJournals<'a>,
}

impl<'a> Batch<'a> {
    /// Decodes one `Batch` from a wire buffer (zero-copy, advances `buf`).
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            blue_score: buf.le_u64("blue_score")?,
            daa_score: buf.le_u64("daa_score")?,
            parent_timestamp: buf.le_u64("parent_timestamp")?,
            prev_lane_tip: buf.array::<32>("prev_lane_tip")?,
            lane_blue_score: buf.le_u64("lane_blue_score")?,
            lane_expired: buf.byte("lane_expired")? != 0,
            parent_seq_commit: buf.array::<32>("parent_seq_commit")?,
            translation: buf.many("translation", |b| b.le_u32("translation"))?,
            tx_journals: TransactionJournals::new(buf.blob("tx_journals")?),
        })
    }

    /// Encodes one batch to bytes (host-side).
    #[cfg(feature = "host")]
    pub fn encode(buf: &mut Vec<u8>, (metadata, translation, tx_journals): BundlePart<'_>) {
        buf.write(&metadata.blue_score.to_le_bytes());
        buf.write(&metadata.daa_score.to_le_bytes());
        buf.write(&metadata.prev_timestamp.to_le_bytes());
        buf.write(&metadata.prev_lane_tip);
        buf.write(&metadata.lane_blue_score.to_le_bytes());
        buf.write(&[if metadata.lane_expired { 1 } else { 0 }]);
        buf.write(&metadata.prev_seq_commit.as_bytes());
        buf.write_many(translation, |&idx| idx.to_le_bytes());
        buf.write(&tx_journals.iter().map(|j| 4 + j.len() as u32).sum::<u32>().to_le_bytes());
        for journal in tx_journals {
            buf.write_blob(journal);
        }
    }
}
