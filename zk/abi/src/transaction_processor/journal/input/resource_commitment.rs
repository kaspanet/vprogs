use crate::{Parser, Result, Write};

/// A single resource's input commitment: its index, identity, and data hash.
pub struct InputResourceCommitment<'a> {
    /// Per-batch resource index.
    pub resource_index: u32,
    /// Unique identifier of this resource.
    pub resource_id: &'a [u8; 32],
    /// BLAKE3 hash of the resource data (or empty leaf hash if no data).
    pub hash: &'a [u8; 32],
}

impl<'a> InputResourceCommitment<'a> {
    /// Wire size of the full encoding: resource_index(4) + resource_id(32) + hash(32).
    pub const SIZE: usize = 4 + 32 + 32;
    /// Wire size without the index prefix: resource_id(32) + hash(32).
    pub const PRE_INDEXED_SIZE: usize = Self::SIZE - 4;

    /// Decodes the full wire format, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        // Parse fields.
        let resource_index = buf[0..4].parse_u32("resource_index")?;
        let resource_id = buf[4..36].parse_into("resource_id")?;
        let hash = buf[36..68].parse_into("hash")?;

        // Advance past consumed bytes.
        *buf = &buf[Self::SIZE..];

        Ok(Self { resource_index, resource_id, hash })
    }

    /// Decodes without the index prefix, advancing `buf` past the consumed bytes.
    pub fn decode_pre_indexed(buf: &mut &'a [u8], resource_index: u32) -> Result<Self> {
        // Parse fields (index provided by caller).
        let resource_id = buf[0..32].parse_into("resource_id")?;
        let hash = buf[32..64].parse_into("hash")?;

        // Advance past consumed bytes.
        *buf = &buf[Self::PRE_INDEXED_SIZE..];

        Ok(Self { resource_index, resource_id, hash })
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
