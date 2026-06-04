use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned,
    little_endian::{U16, U32},
};

/// Zero-copy POD view of a single leaf entry in the proof wire format.
#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct Leaf {
    /// Tree depth at which this leaf sits (shortcut depth).
    pub depth: U16,
    /// Index of this leaf's key in the proof's `keys` table.
    pub key_idx: U32,
    /// Hash of the leaf's value, or `EMPTY_HASH` for absent keys.
    pub value_hash: [u8; 32],
}
