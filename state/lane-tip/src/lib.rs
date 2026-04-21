use vprogs_storage_types::{StateSpace, Store, WriteBatch};

/// Provides type-safe operations for the `LaneTip` column family.
///
/// Stores the post-batch `lane_tip` hash (32 bytes) produced by the batch processor journal,
/// keyed by `batch_index (u64 BE)`. The scheduler writes this under the same [`WriteBatch`] that
/// commits state diffs, so the lane tip is atomic with the corresponding state transition. On
/// rollback, entries for reverted batches are deleted.
pub struct LaneTip;

impl LaneTip {
    /// Returns the persisted lane tip for a given batch index, or `None` if absent. A missing
    /// entry for `batch_index = 0` means we're at the genesis state; callers should treat that
    /// as the zero hash.
    pub fn get<S: Store>(store: &S, batch_index: u64) -> Option<[u8; 32]> {
        store.get(StateSpace::LaneTip, &batch_index.to_be_bytes()).map(|bytes| {
            bytes
                .as_slice()
                .try_into()
                .expect("corrupted store: lane_tip value must be exactly 32 bytes")
        })
    }

    /// Stores the post-batch lane tip for a given batch index.
    pub fn set<W: WriteBatch>(wb: &mut W, batch_index: u64, lane_tip: &[u8; 32]) {
        wb.put(StateSpace::LaneTip, &batch_index.to_be_bytes(), lane_tip);
    }

    /// Deletes the lane tip entry for a single batch index. Used on rollback.
    pub fn delete<W: WriteBatch>(wb: &mut W, batch_index: u64) {
        wb.delete(StateSpace::LaneTip, &batch_index.to_be_bytes());
    }
}
