use alloc::vec::Vec;

use crate::AccessMetadata;

/// A transaction `T` paired with its declared resource accesses and its L1 merge index.
#[derive(Clone, Debug)]
pub struct SchedulerTransaction<T> {
    /// L1 block-wide position of this tx.
    pub merge_idx: u32,
    /// Resources this tx reads or writes.
    pub resources: Vec<AccessMetadata>,
    /// Opaque transaction payload.
    pub tx: T,
}

impl<T> SchedulerTransaction<T> {
    /// Constructs a new [`SchedulerTransaction`].
    pub fn new(merge_idx: u32, resources: Vec<AccessMetadata>, tx: T) -> Self {
        Self { merge_idx, resources, tx }
    }
}
