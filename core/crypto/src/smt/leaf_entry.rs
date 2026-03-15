/// A single leaf in a multi-proof: its depth in the tree, key, and value hash.
///
/// Internal to proof generation — the public API exposes proofs as encoded byte buffers.
pub(crate) struct LeafEntry {
    pub(crate) depth: u16,
    pub(crate) key: [u8; 32],
    pub(crate) value_hash: [u8; 32],
}
