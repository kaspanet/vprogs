//! Writes a `TreeUpdateBatch` into a `WriteBatch` for atomic commit with other state operations.

use vprogs_storage_types::{StateSpace, WriteBatch};

use crate::tree_update_batch::TreeUpdateBatch;

/// Writes SMT node updates into an existing `WriteBatch`.
///
/// Called during `ScheduledBatch::commit()` to persist new and stale SMT nodes atomically
/// alongside state diffs, latest pointers, and batch metadata.
pub struct SmtCommit;

impl SmtCommit {
    /// Writes all new nodes and stale markers from a `TreeUpdateBatch` into the given
    /// `WriteBatch`.
    pub fn write_all<W: WriteBatch>(wb: &mut W, batch: &TreeUpdateBatch) {
        // Write new nodes to the SmtNode CF.
        for (node_key, version, data) in &batch.new_nodes {
            let key = node_key.encode_cf_key(*version);
            wb.put(StateSpace::SmtNode, &key, &data.to_bytes());
        }

        // Write stale markers to the SmtStale CF.
        for stale in &batch.stale_nodes {
            let key = stale.encode_cf_key();
            wb.put(StateSpace::SmtStale, &key, &stale.node_version.to_be_bytes());
        }
    }
}
