use tap::Tap;
use vprogs_core_codec::{Reader, Writer};
use vprogs_core_types::{AccessMetadata, SchedulerTransaction};
use vprogs_l1_types::L1Transaction;

/// Test helpers for [`L1Transaction`].
pub trait L1TransactionExt: Sized {
    /// Builds a v1 L1 transaction whose payload is the wire-encoded `(access_metadata, ix_data)`
    /// pair that the L2 framework expects: `u32 LE count || (id || access_type)*count || ix_data`.
    fn for_l2_test(access_metadata: &[AccessMetadata], ix_data: &[u8]) -> Self;

    /// Re-parses the access metadata prefix from the payload and wraps `self` into a
    /// [`SchedulerTransaction`] - the payload is the single source of truth for the declared
    /// access list.
    fn into_scheduler_tx(self, index: u32) -> SchedulerTransaction<Self>;
}

impl L1TransactionExt for L1Transaction {
    fn for_l2_test(access_metadata: &[AccessMetadata], ix_data: &[u8]) -> Self {
        L1Transaction::default().tap_mut(|tx| {
            tx.version = 1;
            tx.payload = Vec::new().tap_mut(|p| {
                p.write_many(access_metadata, AccessMetadata::encode);
                p.write(ix_data);
            });
        })
    }

    fn into_scheduler_tx(self, index: u32) -> SchedulerTransaction<Self> {
        let access_metadata = self
            .payload
            .as_slice()
            .many("access_metadata", AccessMetadata::decode)
            .expect("malformed test payload");
        SchedulerTransaction::new(index, access_metadata, self)
    }
}
