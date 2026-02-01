use crate::BlockHash;

/// A position in the chain: block hash and sequential index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainStateCoordinate(BlockHash, u64);

impl ChainStateCoordinate {
    /// Creates a new chain coordinate.
    pub fn new(hash: BlockHash, index: u64) -> Self {
        Self(hash, index)
    }

    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
        self.0
    }

    /// Returns the sequential index.
    pub fn index(&self) -> u64 {
        self.1
    }
}
