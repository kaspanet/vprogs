use vprogs_core_codec::Writer;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian::U16};

/// Zero-copy POD view of a single leaf entry in the proof wire format.
#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct Leaf {
    /// Tree depth at which this leaf sits (shortcut depth).
    pub depth: U16,
    /// The full 256-bit key this leaf represents.
    pub key: [u8; 32],
    /// Hash of the leaf's value, or `EMPTY_HASH` for absent keys.
    pub value_hash: [u8; 32],
}

impl Leaf {
    /// Encodes a leaf entry, appending to `buf`.
    pub(crate) fn encode(buf: &mut impl Writer, depth: u16, key: &[u8; 32], value_hash: &[u8; 32]) {
        buf.write(&depth.to_le_bytes());
        buf.write(key);
        buf.write(value_hash);
    }
}
