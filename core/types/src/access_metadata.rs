use alloc::vec::Vec;

use vprogs_core_codec::{Error, Reader, Result};
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

use crate::{AccessType, ResourceId};

/// A transaction's declared access to a single resource.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[derive(TryFromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct AccessMetadata {
    /// Unique identifier of the accessed resource.
    pub resource_id: ResourceId,
    /// Whether this is a read or write access.
    pub access_type: AccessType,
}

impl AccessMetadata {
    /// Constructs a read access entry for `resource_id`.
    pub fn read(resource_id: ResourceId) -> Self {
        Self { resource_id, access_type: AccessType::Read }
    }

    /// Constructs a write access entry for `resource_id`.
    pub fn write(resource_id: ResourceId) -> Self {
        Self { resource_id, access_type: AccessType::Write }
    }

    /// Decodes a length-prefixed list zero-copy, verifying strict-ascending order by `resource_id`.
    pub fn decode_slice<'a>(buf: &mut &'a [u8]) -> Result<&'a [Self]> {
        let view = buf.slice_as::<Self>("access_metadata")?;
        for w in view.windows(2) {
            if w[0].resource_id >= w[1].resource_id {
                return Err(Error::Decode("access metadata not strictly ascending"));
            }
        }

        Ok(view)
    }

    /// Owned variant of [`Self::decode_slice`].
    pub fn decode_vec(buf: &mut &[u8]) -> Result<Vec<Self>> {
        Ok(Self::decode_slice(buf)?.to_vec())
    }
}
