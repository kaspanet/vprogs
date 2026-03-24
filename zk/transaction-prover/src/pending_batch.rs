use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

use crate::TransactionBackend;

/// Per-batch proving state: accumulates receipts in the transaction prover, then moves to the
/// batch prover for assembly once complete.
pub struct PendingBatch<P: Processor<S>, B: TransactionBackend, S: Store> {
    pub batch: ScheduledBatch<S, P>,
    pub receipts: Vec<Option<B::Receipt>>,
    pub(crate) pending: u32,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> PendingBatch<P, B, S> {
    pub(crate) fn new(batch: ScheduledBatch<S, P>) -> Self {
        let tx_count = batch.txs().len();
        Self { batch, receipts: vec![None; tx_count], pending: tx_count as u32 }
    }
}
