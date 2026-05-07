use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_types::{AccessMetadata, SchedulerTransaction};
use vprogs_l1_types::L1Transaction;
use zerocopy::IntoBytes;

/// Test helpers for [`L1Transaction`].
pub trait L1TransactionExt: Sized {
    /// Builds a v1 L1 transaction with the L2-formatted payload (sorts `access_metadata`).
    fn for_l2_test(access_metadata: &[AccessMetadata], ix_data: &[u8]) -> Self;

    /// Wraps `self` into a [`SchedulerTransaction`], re-parsing the access metadata from the
    /// payload as the single source of truth.
    fn into_scheduler_tx(self, merge_idx: u32) -> SchedulerTransaction<Self>;
}

impl L1TransactionExt for L1Transaction {
    fn for_l2_test(access_metadata: &[AccessMetadata], ix_data: &[u8]) -> Self {
        let meta = access_metadata.to_vec().tap_mut(|s| s.sort_unstable_by_key(|m| m.resource_id));

        L1Transaction::default().tap_mut(|tx| {
            tx.version = 1;
            tx.payload = Vec::new().tap_mut(|p| {
                p.write_many(&meta, AccessMetadata::as_bytes);
                p.write(ix_data);
            });
        })
    }

    fn into_scheduler_tx(self, merge_idx: u32) -> SchedulerTransaction<Self> {
        SchedulerTransaction::new(
            merge_idx,
            AccessMetadata::decode_vec(&mut self.payload.as_slice()).expect("unsorted access meta"),
            self,
        )
    }
}
