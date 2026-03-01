use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction payload `T` paired with pre-parsed access metadata, ready for scheduling.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct SchedulerTransaction<T> {
    pub tx: T,
    pub resources: Vec<AccessMetadata>,
}

impl<T> SchedulerTransaction<T> {
    pub fn new(tx: T, resources: Vec<AccessMetadata>) -> Self {
        Self { tx, resources }
    }
}
