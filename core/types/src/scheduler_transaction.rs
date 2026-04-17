use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction payload `T` paired with pre-parsed access metadata, ready for scheduling.
#[derive(Clone, Debug)]
#[derive(BorshSerialize, BorshDeserialize)]
pub struct SchedulerTransaction<T> {
    pub tx: T,
    pub resources: Vec<AccessMetadata>,
}

impl<T> SchedulerTransaction<T> {
    pub fn new(tx: T, resources: Vec<AccessMetadata>) -> Self {
        Self { tx, resources }
    }

    /// Extracts borsh-encoded `Vec<AccessMetadata>` prefix from a payload, returning the decoded
    /// resources and the remaining bytes as the L2 payload. On decode failure, returns empty
    /// resources and an empty L2 payload.
    pub fn extract_payload(payload: &[u8]) -> (Vec<AccessMetadata>, Vec<u8>) {
        let mut cursor = payload;
        match Vec::<AccessMetadata>::deserialize(&mut cursor) {
            Ok(resources) => (resources, cursor.to_vec()),
            Err(_) => (Vec::new(), Vec::new()),
        }
    }
}
