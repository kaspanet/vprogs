use vprogs_core_utils::{DecodeError, Parser};

/// Zero-copy view of a single leaf entry in the proof wire format.
///
/// Fields are materialized at decode time from a 66-byte slice
/// (`depth(2) + key(32) + value_hash(32)`), so field access is free.
pub struct Leaf<'a> {
    pub depth: u16,
    pub key: &'a [u8; 32],
    pub value_hash: &'a [u8; 32],
}

impl<'a> Leaf<'a> {
    /// Size of a single leaf entry in the wire format: depth(2) + key(32) + value_hash(32).
    pub const SIZE: usize = 66;

    /// Decodes a leaf entry, advancing `buf` past the consumed bytes.
    pub(crate) fn decode(buf: &mut &'a [u8]) -> Result<Self, DecodeError> {
        Ok(Self {
            depth: buf.consume_u16("depth")?,
            key: buf.consume_array::<32>("key")?,
            value_hash: buf.consume_array::<32>("value_hash")?,
        })
    }
}
