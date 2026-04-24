use vprogs_core_codec::Reader;

use crate::{Result, Write};

/// Batch-level metadata decoded from the wire header.
///
/// Kept intentionally small: only fields a transaction guest actually reads or needs for
/// cross-tx consistency checks in the batch processor. Everything else (timestamps, scores,
/// parent refs) is per-block context consumed only by the batch processor and plumbed through
/// its top-level inputs, not committed per-tx.
#[derive(Clone, Copy)]
pub struct BatchMetadata<'a> {
    /// L1 block hash this tx was accepted in.
    pub block_hash: &'a [u8; 32],
    /// Mergeset context hash for the chain block - exposes the kip21 context digest to the VM as
    /// a source of on-chain randomness and anchors cross-tx consistency within a block group.
    pub context_hash: &'a [u8; 32],
}

impl<'a> BatchMetadata<'a> {
    /// Wire size: block_hash(32) + context_hash(32).
    pub const SIZE: usize = 32 + 32;

    /// Decodes batch metadata, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            block_hash: buf.array::<32>("block_hash")?,
            context_hash: buf.array::<32>("context_hash")?,
        })
    }

    /// Encodes batch metadata to the given writer.
    pub fn encode(&self, w: &mut impl Write) {
        w.write(self.block_hash);
        w.write(self.context_hash);
    }
}
