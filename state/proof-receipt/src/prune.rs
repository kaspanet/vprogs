use vprogs_storage_types::{StateSpace, Store, WriteBatch};
use zerocopy::IntoBytes;

use crate::Prefix;

/// Deletes every cached receipt at `prefix`'s checkpoint index, across all programs.
///
/// Reorg recovery calls this to drop the receipts a reverted checkpoint invalidated: one call
/// removes every batch, tx, and aggregator receipt for that checkpoint, whatever its image id
/// (for the aggregator, the checkpoint index is its bundle-start index, so a bundle that began
/// at the reverted checkpoint is dropped). Taking a [`Prefix`] keeps the iteration prefix at
/// exactly the column family's extractor length.
pub fn invalidate_checkpoint<S>(store: &S, wb: &mut S::WriteBatch, prefix: &Prefix)
where
    S: Store,
{
    for (key, _) in store.prefix_iter(StateSpace::ProofReceipt, prefix.as_bytes()) {
        wb.delete(StateSpace::ProofReceipt, &key);
    }
}
