use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_transaction_prover::TransactionBackend;

/// Per-batch proving state: accumulates individual transaction receipts until all are collected,
/// then moves to assembly and batch proving.
pub(crate) struct Input<P: Processor<S>, B: TransactionBackend, S: Store> {
    pub(crate) batch: ScheduledBatch<S, P>,
    pub(crate) receipts: Vec<Option<B::Receipt>>,
    pub(crate) pending: u32,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> Input<P, B, S> {
    pub(crate) fn new(batch: ScheduledBatch<S, P>) -> Self {
        let tx_count = batch.txs().len();
        Self { batch, receipts: vec![None; tx_count], pending: tx_count as u32 }
    }
}
