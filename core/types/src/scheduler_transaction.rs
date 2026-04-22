use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction payload `T` paired with pre-parsed access metadata and a batch-unique index,
/// ready for scheduling. Indices must be strictly monotonic across a batch (the batch processor
/// enforces this); upstream pipelines supply the tx's block-wide position so it doubles as the
/// kip21 seq-commit `activity_leaf(tx_id, version, merge_idx)` input.
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
