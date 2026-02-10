use borsh::{BorshDeserialize, BorshSerialize};

use crate::BatchMetadata;

/// A saved batch position identified by its sequential index and associated metadata.
///
/// Used as the return type for queries like `last_committed` and `last_pruned`.
#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct Checkpoint<M: BatchMetadata> {
    /// Sequential batch index.
    index: u64,
    /// Associated batch metadata.
    metadata: M,
}

impl<M: BatchMetadata> Checkpoint<M> {
    /// Creates a checkpoint from the given index and metadata.
    pub fn new(index: u64, metadata: M) -> Self {
        Self { index, metadata }
    }

    /// Returns the sequential batch index.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Returns a reference to the associated metadata.
    pub fn metadata(&self) -> &M {
        &self.metadata
    }
}
