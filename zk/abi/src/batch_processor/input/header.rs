use alloc::vec::Vec;

use crate::{Parser, Result};

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

    /// Decodes the header from the start of a buffer.
    pub fn decode(buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            image_id: buf[0..32].parse_into("image_id")?,
            batch_index: buf[32..40].parse_u64("batch_index")?,
            prev_root: buf[40..72].parse_into("prev_root")?,
            n_resources: buf[72..76].parse_u32("n_resources")?,
            n_txs: buf[76..80].parse_u32("n_txs")?,
        })
    }

    /// Encodes the header into a buffer.
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.image_id);
        buf.extend_from_slice(&self.batch_index.to_le_bytes());
        buf.extend_from_slice(self.prev_root);
        buf.extend_from_slice(&self.n_resources.to_le_bytes());
        buf.extend_from_slice(&self.n_txs.to_le_bytes());
    }
}
