/// A single leaf in a multi-proof: its depth in the tree, key, and value hash.
pub struct LeafEntry {
    pub depth: u16,
    pub key: [u8; 32],
    pub value_hash: [u8; 32],
}
