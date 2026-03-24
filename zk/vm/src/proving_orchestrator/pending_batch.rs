use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

use crate::Backend;

/// Per-batch proving state: accumulates receipts in the transaction prover, then moves to the
/// batch prover for assembly once complete.
pub(crate) struct PendingBatch<P: Processor<S>, B: Backend, S: Store> {
    pub(crate) batch: ScheduledBatch<S, P>,
    pub(crate) receipts: Vec<Option<B::Receipt>>,
    pub(crate) pending: u32,
}

impl<P: Processor<S>, B: Backend, S: Store> PendingBatch<P, B, S> {
    pub(crate) fn new(batch: ScheduledBatch<S, P>) -> Self {
        let tx_count = batch.txs().len();
        Self { batch, receipts: vec![None; tx_count], pending: tx_count as u32 }
    }
}
