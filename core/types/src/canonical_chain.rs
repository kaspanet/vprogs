/// Read interface for the canonical chain: the block hash canonical at each batch index.
pub trait CanonicalChain: Clone + Send + Sync + 'static {
    /// Returns the canonical block hash at `index`, or `None` if outside the live range.
    fn block(&self, index: u64) -> Option<[u8; 32]>;

    /// Returns whether `block_hash` is the canonical batch at `index`.
    fn is_canonical(&self, index: u64, block_hash: &[u8; 32]) -> bool {
        self.block(index).as_ref() == Some(block_hash)
    }
}
