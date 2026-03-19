use vprogs_core_codec::Reader;

use crate::{Result, Write};

/// Decoded batch processor input header.
pub struct Header<'a> {
    /// Transaction processor guest image ID.
    pub image_id: &'a [u8; 32],
    /// Monotonic batch sequence number.
    pub batch_index: u64,
    /// State root before this batch is applied.
    pub prev_root: &'a [u8; 32],
    /// Total number of unique resources across all transactions.
    pub n_resources: u32,
    /// Number of transactions in this batch.
    pub n_txs: u32,
}

impl<'a> Header<'a> {
    /// Wire size of the header: image_id(32) + batch_index(8) + prev_root(32) + n_resources(4) +
    /// n_txs(4).
    pub const SIZE: usize = 32 + 8 + 32 + 4 + 4;

    /// Decodes the header, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            image_id: buf.array::<32>("image_id")?,
            batch_index: buf.le_u64("batch_index")?,
            prev_root: buf.array::<32>("prev_root")?,
            n_resources: buf.le_u32("n_resources")?,
            n_txs: buf.le_u32("n_txs")?,
        })
    }

    /// Encodes the header into a writer.
    pub fn encode(&self, w: &mut impl Write) {
        w.write(self.image_id);
        w.write(&self.batch_index.to_le_bytes());
        w.write(self.prev_root);
        w.write(&self.n_resources.to_le_bytes());
        w.write(&self.n_txs.to_le_bytes());
    }
}
