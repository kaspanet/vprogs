use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction payload `T` paired with pre-parsed access metadata and a batch-unique index.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct SchedulerTransaction<T> {
    pub index: u32,
    pub resources: Vec<AccessMetadata>,
    pub tx: T,
}

impl<T> SchedulerTransaction<T> {
    pub fn new(index: u32, resources: Vec<AccessMetadata>, tx: T) -> Self {
        Self { index, resources, tx }
    }
}
