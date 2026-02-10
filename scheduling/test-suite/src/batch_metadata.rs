use borsh::{BorshDeserialize, BorshSerialize};

/// Minimal [`vprogs_core_types::BatchMetadata`] implementation.
///
/// Stores a single `u64` counter, serialized via Borsh.
#[derive(Clone, Debug, Default, PartialEq, BorshSerialize, BorshDeserialize)]
pub struct BatchMetadata(u64);

impl BatchMetadata {
    /// Creates a new instance with the given counter value.
    pub fn new(counter: u64) -> Self {
        Self(counter)
    }
}
