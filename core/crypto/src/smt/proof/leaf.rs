use alloc::vec::Vec;

use vprogs_core_utils::{Parser, Result};

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
    pub(crate) fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            depth: buf.le_u16("depth")?,
            key: buf.array::<32>("key")?,
            value_hash: buf.array::<32>("value_hash")?,
        })
    }

    /// Encodes a leaf entry, appending to `buf`.
    pub(crate) fn encode(buf: &mut Vec<u8>, depth: u16, key: &[u8; 32], value_hash: &[u8; 32]) {
        buf.extend_from_slice(&depth.to_le_bytes());
        buf.extend_from_slice(key);
        buf.extend_from_slice(value_hash);
    }
}
