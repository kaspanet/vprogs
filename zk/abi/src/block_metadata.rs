/// Block-level metadata from the wire header.
///
/// Scalars (`blue_score`) are parsed once during decode. Variable-length fields
/// (`block_hash`) are zero-copy references into the underlying buffer.
pub struct BlockMetadata<'a> {
    pub block_hash: &'a [u8; 32],
    pub blue_score: u64,
}
