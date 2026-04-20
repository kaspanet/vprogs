use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction payload `T` paired with pre-parsed access metadata, ready for scheduling.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct SchedulerTransaction<T> {
    pub resources: Vec<AccessMetadata>,
    pub tx: T,
}

impl<T> SchedulerTransaction<T> {
    pub fn new(resources: Vec<AccessMetadata>, tx: T) -> Self {
        Self { resources, tx }
    }
}
