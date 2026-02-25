use borsh::{BorshDeserialize, BorshSerialize};

/// Batch metadata for the ZK VM.
#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct BatchMetadata {
    pub batch_index: u64,
}
