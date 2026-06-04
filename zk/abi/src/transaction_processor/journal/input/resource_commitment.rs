use vprogs_core_codec::Writer;
use vprogs_core_hashing::Hasher;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_types::ResourceId;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian::U32};

use crate::transaction_processor::Resource;

/// A single resource's input commitment: its index, identity, and data hash.
#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct InputResourceCommitment {
    /// Per-batch resource index.
    pub resource_index: U32,
    /// Unique identifier of this resource.
    pub resource_id: ResourceId,
    /// Hash of the resource data, computed via the SMT's configured [`Hasher`].
    pub hash: [u8; 32],
}

impl InputResourceCommitment {
    /// Encodes a resource's input commitment to the journal, hashing the data with `H`.
    pub fn encode<H: Hasher>(w: &mut impl Writer, r: &Resource<'_>) {
        w.write(&r.index().to_le_bytes());
        w.write(r.id().as_slice());
        match r.data() {
            [] => w.write(&EMPTY_HASH),
            data => w.write(&H::hash(data)),
        }
    }
}
