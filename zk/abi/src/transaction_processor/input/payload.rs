use vprogs_core_types::AccessMetadata;

use crate::Result;

/// Decoded view of an L2 transaction payload.
pub struct Payload<'a> {
    /// Full payload bytes.
    pub bytes: &'a [u8],
    /// Resources this transaction declares it will access.
    pub access_metadata: &'a [AccessMetadata],
    /// Opaque instruction data.
    pub ix_data: &'a [u8],
}

impl<'a> Payload<'a> {
    /// Parses a payload, splitting the access metadata prefix from the trailing instruction data.
    pub fn decode(mut bytes: &'a [u8]) -> Result<Self> {
        Ok(Self {
            bytes,
            access_metadata: AccessMetadata::decode_slice(&mut bytes)?,
            ix_data: bytes,
        })
    }
}
