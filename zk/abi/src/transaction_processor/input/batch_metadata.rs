use vprogs_core_codec::Reader;

use crate::{Result, Write};

/// Batch-level metadata decoded from the wire header.
pub struct BatchMetadata<'a> {
    /// The hash of the block this batch belongs to.
    pub block_hash: &'a [u8; 32],
    /// The DAG blue score of the block.
    pub blue_score: u64,
}

impl<'a> BatchMetadata<'a> {
    /// Wire size: block_hash(32) + blue_score(8).
    pub const SIZE: usize = 32 + 8;

    /// Decodes batch metadata, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            block_hash: buf.array::<32>("block_hash")?,
            blue_score: buf.le_u64("blue_score")?,
        })
    }

    /// Encodes batch metadata to the given writer.
    pub fn encode(&self, w: &mut impl Write) {
        w.write(self.block_hash);
        w.write(&self.blue_score.to_le_bytes());
    }
}
