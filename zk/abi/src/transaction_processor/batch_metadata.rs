use crate::Write;

/// Batch-level metadata decoded from the wire header.
///
/// Scalars are parsed once; `block_hash` is a zero-copy reference into the buffer.
pub struct BatchMetadata<'a> {
    /// The hash of the block this batch belongs to.
    pub block_hash: &'a [u8; 32],
    /// The DAG blue score of the block.
    pub blue_score: u64,
}

impl<'a> BatchMetadata<'a> {
    /// Wire size: block_hash(32) + blue_score(8).
    pub const SIZE: usize = 32 + 8;

    /// Decodes batch metadata from the wire header at the given offset.
    pub(crate) fn decode(buf: &'a [u8]) -> Self {
        let block_hash: &[u8; 32] = buf[0..32].try_into().expect("truncated header");
        let blue_score = u64::from_le_bytes(buf[32..40].try_into().expect("truncated header"));
        Self { block_hash, blue_score }
    }

    /// Encodes batch metadata to the given writer.
    pub(crate) fn encode(&self, w: &mut impl Write) {
        w.write(self.block_hash);
        w.write(&self.blue_score.to_le_bytes());
    }
}
