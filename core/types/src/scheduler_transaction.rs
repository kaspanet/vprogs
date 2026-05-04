use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction `T` paired with its declared resource accesses and a batch-unique index.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct SchedulerTransaction<T> {
    /// Strictly-increasing position of this tx within its batch.
    pub index: u32,
    /// Resources this tx reads or writes.
    pub resources: Vec<AccessMetadata>,
    /// Opaque transaction payload.
    pub tx: T,
}

impl<T> SchedulerTransaction<T> {
    /// Constructs a new [`SchedulerTransaction`].
    pub fn new(index: u32, resources: Vec<AccessMetadata>, tx: T) -> Self {
        Self { index, resources, tx }
    }
}
