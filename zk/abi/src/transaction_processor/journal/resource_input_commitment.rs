use crate::Write;

/// A single resource's input commitment: its index, identity, and data hash.
pub struct ResourceInputCommitment<'a> {
    pub resource_index: u32,
    pub resource_id: &'a [u8; 32],
    pub hash: &'a [u8; 32],
}

impl<'a> ResourceInputCommitment<'a> {
    /// Wire size of the full encoding: resource_index(4) + resource_id(32) + hash(32).
    pub const SIZE: usize = 4 + 32 + 32;

    /// Decodes the full wire format: `resource_index(4) + resource_id(32) + hash(32)`.
    pub fn decode(buf: &'a [u8]) -> Self {
        Self {
            resource_index: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            resource_id: buf[4..36].try_into().unwrap(),
            hash: buf[36..68].try_into().unwrap(),
        }
    }

    /// Decodes with a pre-determined index: `resource_id(32) + hash(32)`.
    pub fn decode_pre_indexed(buf: &'a [u8], resource_index: u32) -> Self {
        Self {
            resource_index,
            resource_id: buf[0..32].try_into().unwrap(),
            hash: buf[32..64].try_into().unwrap(),
        }
    }

    /// Encodes the full wire format: `resource_index(4) + resource_id(32) + hash(32)`.
    pub fn encode(&self, w: &mut impl Write) {
        w.write(&self.resource_index.to_le_bytes());
        self.encode_pre_indexed(w);
    }

    /// Encodes without the index: `resource_id(32) + hash(32)`.
    pub fn encode_pre_indexed(&self, w: &mut impl Write) {
        w.write(self.resource_id);
        w.write(self.hash);
    }
}
