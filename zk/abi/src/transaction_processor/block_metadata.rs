/// Block-level metadata decoded from the wire header.
///
/// Scalars are parsed once; `block_hash` is a zero-copy reference into the buffer.
pub struct BlockMetadata<'a> {
    /// The hash of the block this transaction belongs to.
    pub block_hash: &'a [u8; 32],
    /// The DAG blue score of the block.
    pub blue_score: u64,
}
