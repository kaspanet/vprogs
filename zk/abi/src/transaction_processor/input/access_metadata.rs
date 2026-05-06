use vprogs_core_codec::Reader;
use vprogs_core_types::{AccessType, ResourceId};

/// Typed view of a single access metadata entry decoded from a transaction payload's resource
/// prefix. `resource_id` borrows directly into the payload bytes; `access_type` is validated
/// during decode.
///
/// Wire layout matches the host encoder
/// [`vprogs_core_types::AccessMetadata::encode`](vprogs_core_types::AccessMetadata::encode):
/// `resource_id(32) || access_type(1)`.
#[derive(Clone, Copy)]
pub struct AccessMetadata<'a> {
    /// Zero-copy reference into the payload's resource id bytes.
    pub resource_id: &'a ResourceId,
    /// L2 access type for this resource.
    pub access_type: AccessType,
}

impl<'a> AccessMetadata<'a> {
    /// Decodes a single entry from the wire layout `resource_id(32) || access_type(1)`.
    pub fn decode(buf: &mut &'a [u8]) -> vprogs_core_codec::Result<Self> {
        Ok(Self {
            resource_id: buf.array::<32>("resource_id")?.into(),
            access_type: AccessType::try_from(buf.byte("access_type")?)?,
        })
    }
}
