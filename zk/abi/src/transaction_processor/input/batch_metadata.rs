use vprogs_core_codec::Reader;

use crate::{Result, Write};

/// Batch-level metadata decoded from the wire header.
#[derive(Clone, Copy)]
pub struct BatchMetadata<'a> {
    /// L1 block hash.
    pub block_hash: &'a [u8; 32],
    /// This block's sequencing commitment (accepted-id merkle root) - what the on-chain
    /// `OpChainblockSeqCommit(block_hash)` returns.
    pub seq_commit: &'a [u8; 32],
    /// Parent block's sequencing commitment - the chain anchor for settlement continuity.
    pub prev_seq_commit: &'a [u8; 32],
    /// DAG blue score at this block's position.
    pub blue_score: u64,
    /// DAA score at this block's position.
    pub daa_score: u64,
    /// Block header timestamp in milliseconds.
    pub timestamp: u64,
    /// Previous block's header timestamp in milliseconds.
    pub prev_timestamp: u64,
}

impl<'a> BatchMetadata<'a> {
    /// Wire size: block_hash(32) + seq_commit(32) + prev_seq_commit(32) + blue_score(8)
    /// + daa_score(8) + timestamp(8) + prev_timestamp(8).
    pub const SIZE: usize = 32 + 32 + 32 + 8 + 8 + 8 + 8;

    /// Decodes batch metadata, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            block_hash: buf.array::<32>("block_hash")?,
            seq_commit: buf.array::<32>("seq_commit")?,
            prev_seq_commit: buf.array::<32>("prev_seq_commit")?,
            blue_score: buf.le_u64("blue_score")?,
            daa_score: buf.le_u64("daa_score")?,
            timestamp: buf.le_u64("timestamp")?,
            prev_timestamp: buf.le_u64("prev_timestamp")?,
        })
    }

    /// Encodes batch metadata to the given writer.
    pub fn encode(&self, w: &mut impl Write) {
        w.write(self.block_hash);
        w.write(self.seq_commit);
        w.write(self.prev_seq_commit);
        w.write(&self.blue_score.to_le_bytes());
        w.write(&self.daa_score.to_le_bytes());
        w.write(&self.timestamp.to_le_bytes());
        w.write(&self.prev_timestamp.to_le_bytes());
    }
}
