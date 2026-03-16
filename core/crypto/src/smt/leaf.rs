/// Zero-copy view of a single leaf entry in the proof wire format.
///
/// Fields are materialized at construction time from a 66-byte slice
/// (`depth(2) + key(32) + value_hash(32)`), so accessors are free.
pub struct Leaf<'a> {
    pub depth: u16,
    pub key: &'a [u8; 32],
    pub value_hash: &'a [u8; 32],
}

impl<'a> Leaf<'a> {
    /// Size of a single leaf entry in the wire format: depth(2) + key(32) + value_hash(32).
    pub const SIZE: usize = 66;

    /// Decodes a 66-byte slice into a leaf entry.
    pub(crate) fn decode(buf: &'a [u8]) -> Self {
        Self {
            depth: u16::from_le_bytes(buf[..2].try_into().expect("truncated depth")),
            key: buf[2..][..32].try_into().expect("truncated key"),
            value_hash: buf[34..][..32].try_into().expect("truncated value_hash"),
        }
    }
}
