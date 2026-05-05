use vprogs_core_codec::{Error, Reader, Result};

use crate::{AccessType, ResourceId};

/// Wire size of a single encoded `AccessMetadata`: 32-byte resource id + 1-byte access type.
pub const ACCESS_METADATA_WIRE_SIZE: usize = 33;

#[derive(Clone, Copy, Debug)]
pub struct AccessMetadata {
    pub resource_id: ResourceId,
    pub access_type: AccessType,
}

impl AccessMetadata {
    pub fn read(resource_id: ResourceId) -> Self {
        Self { resource_id, access_type: AccessType::Read }
    }

    pub fn write(resource_id: ResourceId) -> Self {
        Self { resource_id, access_type: AccessType::Write }
    }

    /// Decodes a single entry from the wire layout `resource_id(32) || access_type(1)`.
    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            resource_id: ResourceId::from(*buf.array::<32>("resource_id")?),
            access_type: match buf.byte("access_type")? {
                0 => AccessType::Read,
                1 => AccessType::Write,
                _ => return Err(Error::Decode("access_type")),
            },
        })
    }

    /// Encodes a single entry as `resource_id(32) || access_type(1)`.
    pub fn encode(&self) -> [u8; ACCESS_METADATA_WIRE_SIZE] {
        let mut out = [0u8; ACCESS_METADATA_WIRE_SIZE];
        out[..32].copy_from_slice(&*self.resource_id);
        out[32] = self.access_type as u8;
        out
    }
}
