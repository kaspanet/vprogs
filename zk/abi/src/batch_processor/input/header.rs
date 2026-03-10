use alloc::vec::Vec;

/// Decoded batch processor input header.
pub struct Header<'a> {
    pub image_id: &'a [u8; 32],
    pub batch_index: u64,
    pub prev_root: &'a [u8; 32],
    pub n_resources: u32,
    pub n_txs: u32,
}

impl<'a> Header<'a> {
    /// image_id(32) + batch_index(8) + prev_root(32) + n_resources(4) + n_txs(4).
    pub const SIZE: usize = 32 + 8 + 32 + 4 + 4;

    /// Decodes the header from the start of a buffer.
    pub fn decode(buf: &'a [u8]) -> Self {
        assert!(buf.len() >= Self::SIZE, "input too short for header");

        Self {
            image_id: buf[0..32].try_into().unwrap(),
            batch_index: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            prev_root: buf[40..72].try_into().unwrap(),
            n_resources: u32::from_le_bytes(buf[72..76].try_into().unwrap()),
            n_txs: u32::from_le_bytes(buf[76..80].try_into().unwrap()),
        }
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
