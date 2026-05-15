use kaspa_hashes::Hash;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
use vprogs_core_codec::{Reader, Result};
#[cfg(feature = "host")]
use zerocopy::IntoBytes;
use zerocopy::{FromBytes, little_endian::U32};

#[cfg(feature = "host")]
use crate::batch_processor::BundlePart;
use crate::batch_processor::TransactionJournals;

/// One batch of a bundle: per-block context plus the lane txs in that block.
pub struct Batch<'a> {
    /// DAG blue score of this batch's chain block.
    pub blue_score: u64,
    /// DAA score of this batch's chain block.
    pub daa_score: u64,
    /// Timestamp of this batch's selected parent block.
    pub prev_timestamp: u64,
    /// `seq_commit` of this block's selected parent (used iff `lane_expired`).
    pub prev_seq_commit: &'a Hash,
    /// Lane tip entering this batch's block.
    pub prev_lane_tip: &'a Hash,
    /// Blue score at which the lane was last active before this block.
    pub prev_lane_blue_score: u64,
    /// True when the lane re-anchors on `prev_seq_commit` instead of `prev_lane_tip`.
    pub lane_expired: bool,
    /// Maps batch-local `resource_index` to bundle-wide resource_index.
    pub translation: &'a [U32],
    /// Per-tx journal entries.
    pub tx_journals: TransactionJournals<'a>,
}

impl<'a> Batch<'a> {
    /// Decodes one `Batch` from a wire buffer (zero-copy, advances `buf`).
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            blue_score: buf.le_u64("blue_score")?,
            daa_score: buf.le_u64("daa_score")?,
            prev_timestamp: buf.le_u64("prev_timestamp")?,
            prev_seq_commit: buf.array_as::<Hash>("prev_seq_commit")?,
            prev_lane_tip: buf.array_as::<Hash>("prev_lane_tip")?,
            prev_lane_blue_score: buf.le_u64("prev_lane_blue_score")?,
            lane_expired: buf.bool("lane_expired")?,
            translation: <[U32]>::ref_from_bytes(buf.blob("translation")?)?,
            tx_journals: TransactionJournals::new(buf.blob("tx_journals")?),
        })
    }

    /// Encodes one batch to bytes (host-side).
    #[cfg(feature = "host")]
    pub fn encode(buf: &mut impl Writer, (metadata, translation, tx_journals): BundlePart<'_>) {
        buf.write(&metadata.blue_score.to_le_bytes());
        buf.write(&metadata.daa_score.to_le_bytes());
        buf.write(&metadata.prev_timestamp.to_le_bytes());
        buf.write(metadata.prev_seq_commit.as_slice());
        buf.write(metadata.prev_lane_tip.as_slice());
        buf.write(&metadata.prev_lane_blue_score.to_le_bytes());
        buf.write(&[metadata.lane_expired as u8]);
        buf.write_blob(translation.as_bytes());
        buf.write(&tx_journals.iter().map(|j| 4 + j.len() as u32).sum::<u32>().to_le_bytes());
        for journal in tx_journals {
            buf.write_blob(journal);
        }
    }
}
