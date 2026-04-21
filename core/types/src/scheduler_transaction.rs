use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction payload `T` paired with pre-parsed access metadata, ready for scheduling.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct SchedulerTransaction<T> {
    pub resources: Vec<AccessMetadata>,
    pub tx: T,
    /// Optional block-wide `merge_idx` override - set by upstream pipelines (e.g. the L1 bridge
    /// node worker) that filter a block's accepted-tx list down to a single lane but need to
    /// preserve each tx's position in the unfiltered list for the kip21 seq-commit
    /// `activity_leaf(tx_id, version, merge_idx)` hash. When `None`, the scheduler assigns the
    /// transaction's position within the batch (0..N).
    pub merge_idx: Option<u32>,
}

impl<T> SchedulerTransaction<T> {
    pub fn new(resources: Vec<AccessMetadata>, tx: T) -> Self {
        Self { resources, tx, merge_idx: None }
    }

    /// Variant that pins a block-wide `merge_idx` for this transaction. See
    /// [`merge_idx`](Self::merge_idx) for the full rationale.
    pub fn with_merge_idx(resources: Vec<AccessMetadata>, tx: T, merge_idx: u32) -> Self {
        Self { resources, tx, merge_idx: Some(merge_idx) }
    }
}
