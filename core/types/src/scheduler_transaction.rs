use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction payload `T` paired with pre-parsed access metadata, ready for scheduling.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct SchedulerTransaction<T> {
    pub tx: T,
    pub resources: Vec<AccessMetadata>,
}

impl<T> SchedulerTransaction<T> {
    pub fn new(tx: T, resources: Vec<AccessMetadata>) -> Self {
        Self { tx, resources }
    }

    /// Decodes the borsh-encoded `Vec<AccessMetadata>` prefix from a payload. On decode failure,
    /// returns empty resources.
    pub fn extract_resources(mut payload: &[u8]) -> Vec<AccessMetadata> {
        Vec::<AccessMetadata>::deserialize(&mut payload).unwrap_or_default()
    }
}
