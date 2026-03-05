/// Immutable transaction metadata from the wire header.
pub struct Metadata<'a> {
    pub tx_index: u32,
    pub tx_bytes: &'a [u8],
    pub block_hash: [u8; 32],
    pub blue_score: u64,
}
