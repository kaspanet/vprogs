use vprogs_core_codec::Reader;

use crate::{Result, Write};

/// Batch-level metadata decoded from the wire header.
///
/// This is the wire projection of [`ChainBlockMetadata`](vprogs_l1_types::ChainBlockMetadata).
/// All fields are shared across every transaction in a batch - the batch processor enforces
/// equality via `check_batch_metadata`. The extra context fields (timestamp, daa_score,
/// selected_parent_timestamp) exist so the kip21 seq-commit `mergeset_context_hash` can be
/// reconstructed from the journal without an out-of-band lookup.
#[derive(Clone, Copy)]
pub struct BatchMetadata<'a> {
    /// The hash of the block this batch belongs to.
    pub block_hash: &'a [u8; 32],
    /// The DAG blue score of the block.
    pub blue_score: u64,
    /// The DAA score of the block.
    pub daa_score: u64,
    /// The block header's own timestamp (milliseconds). Recorded for covenant auditability.
    pub timestamp: u64,
    /// The selected parent's timestamp (milliseconds). This is the value committed by the kip21
    /// seq-commit `mergeset_context_hash`, not the block's own timestamp.
    pub selected_parent_timestamp: u64,
}

impl<'a> BatchMetadata<'a> {
    /// Wire size: block_hash(32) + blue_score(8) + daa_score(8) + timestamp(8) +
    /// selected_parent_timestamp(8).
    pub const SIZE: usize = 32 + 8 + 8 + 8 + 8;

    /// Decodes batch metadata, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            block_hash: buf.array::<32>("block_hash")?,
            blue_score: buf.le_u64("blue_score")?,
            daa_score: buf.le_u64("daa_score")?,
            timestamp: buf.le_u64("timestamp")?,
            selected_parent_timestamp: buf.le_u64("selected_parent_timestamp")?,
        })
    }

    /// Encodes batch metadata to the given writer.
    pub fn encode(&self, w: &mut impl Write) {
        w.write(self.block_hash);
        w.write(&self.blue_score.to_le_bytes());
        w.write(&self.daa_score.to_le_bytes());
        w.write(&self.timestamp.to_le_bytes());
        w.write(&self.selected_parent_timestamp.to_le_bytes());
    }
}
