use crate::BatchMetadata;

/// A saved batch position identified by its sequential index and associated metadata.
///
/// Used as the return type for queries like `last_processed` and `last_pruned`.
/// Centralizes serialization so callers don't repeat index/metadata byte layout.
#[derive(Clone, Default)]
pub struct Checkpoint<M: BatchMetadata> {
    index: u64,
    metadata: M,
}

impl<M: BatchMetadata> Checkpoint<M> {
    pub fn new(index: u64, metadata: M) -> Self {
        Self { index, metadata }
    }

    pub fn index(&self) -> u64 {
        self.index
    }

    pub fn metadata(&self) -> &M {
        &self.metadata
    }

    /// Serializes as `index (8 bytes BE) || metadata bytes`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let metadata_bytes = self.metadata.to_bytes();
        let mut value = Vec::with_capacity(8 + metadata_bytes.len());
        value.extend_from_slice(&self.index.to_be_bytes());
        value.extend_from_slice(&metadata_bytes);
        value
    }

    /// Deserializes from `index (8 bytes BE) || metadata bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let index = u64::from_be_bytes(bytes[..8].try_into().unwrap());
        let metadata = M::from_bytes(&bytes[8..]);
        Self { index, metadata }
    }
}
