use crate::CanonicalChain;

/// Fork-awareness disabled: with no competing forks, every entry is canonical.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpCanonicalChain;

impl CanonicalChain for NoOpCanonicalChain {
    fn block(&self, _index: u64) -> Option<[u8; 32]> {
        None
    }

    fn is_canonical(&self, _index: u64, _block_hash: &[u8; 32]) -> bool {
        true
    }
}
