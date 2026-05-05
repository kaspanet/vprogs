use alloc::vec::Vec;

use vprogs_core_codec::Reader;

use crate::{Result, transaction_processor::AccessMetadata};

/// Zero-copy view of an L2 transaction payload, split into structural components at decode time.
///
/// Wire layout: `access_metadata_prefix || ix_data`. The access metadata is a length-prefixed
/// list of `(resource_id, access_type)` entries declaring the resources this transaction will
/// read or write; `ix_data` is the remaining bytes. The instruction-data schema (Solana-style)
/// is L2-specific and not parsed at this layer.
pub struct Payload<'a> {
    /// Original payload bytes including the access metadata prefix. Used for tx_id derivation.
    pub bytes: &'a [u8],
    /// Resources this transaction declares it will access.
    pub access_metadata: Vec<AccessMetadata<'a>>,
    /// Instruction data - opaque bytes following the access metadata prefix.
    pub ix_data: &'a [u8],
}

impl<'a> Payload<'a> {
    /// Parses a payload from its raw bytes, splitting off the access metadata prefix and exposing
    /// the remainder as opaque instruction data.
    pub fn decode(mut bytes: &'a [u8]) -> Result<Self> {
        Ok(Self {
            bytes,
            access_metadata: bytes.many("access_metadata", AccessMetadata::decode)?,
            ix_data: bytes,
        })
    }
}
