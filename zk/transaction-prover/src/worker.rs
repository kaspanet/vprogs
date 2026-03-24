use std::{collections::HashMap, pin::Pin, thread::JoinHandle};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::runtime::Builder;
use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_abi::transaction_processor::Inputs;

use crate::{
    TransactionBackend, completed_transaction::CompletedTransaction, pending_batch::PendingBatch,
    pending_transaction::PendingTransaction,
};

/// Background worker that dispatches transaction proofs to a [`TransactionBackend`] concurrently,
/// tracks per-batch receipt accumulation, and pushes completed batches to the batch prover.
pub(crate) struct TransactionProverWorker<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// The proof backend used for proving transactions.
    backend: B,
    /// Proof tasks submitted by execution workers.
    inbox: AsyncQueue<PendingTransaction<P, S>>,
    /// Completed batches forwarded to the batch prover.
    outbox: AsyncQueue<PendingBatch<P, B, S>>,
    /// Receipts accumulating per batch (keyed by batch index).
    batches: HashMap<u64, PendingBatch<P, B, S>>,
    /// Transaction proofs currently in flight.
    #[allow(clippy::type_complexity)]
    transactions: FuturesUnordered<
        Pin<Box<dyn futures::Future<Output = CompletedTransaction<P, B, S>> + Send>>,
    >,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> TransactionProverWorker<P, B, S> {
    pub(crate) fn new(
        backend: B,
        inbox: AsyncQueue<PendingTransaction<P, S>>,
        outbox: AsyncQueue<PendingBatch<P, B, S>>,
    ) -> Self {
        Self {
            backend,
            inbox,
            outbox,
            batches: HashMap::new(),
            transactions: FuturesUnordered::new(),
        }
    }

    pub(crate) fn spawn(self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(self.run())
        })
    }

    async fn run(mut self) {
        while !self.inbox.is_singleton() || !self.transactions.is_empty() {
            // Submit all queued transactions for proving.
            while let Some(PendingTransaction { batch, mut tx_inputs }) = self.inbox.pop() {
                if !batch.was_canceled() {
                    if let Ok(Inputs { tx_index, .. }) = Inputs::decode(&mut tx_inputs[..]) {
                        let receipt = self.backend.prove_transaction(tx_inputs);
                        self.transactions.push(Box::pin(async move {
                            CompletedTransaction { batch, index: tx_index, receipt: receipt.await }
                        }));
                    };
                }
            }

            // Wait for either a new task or a completed proof.
            tokio::select! {
                biased;
                () = self.inbox.notified() => {}
                Some(tx) = self.transactions.next(), if !self.transactions.is_empty() => {
                    self.publish_receipt(tx);
                }
            }
        }
    }

    /// Records a completed transaction receipt and forwards the batch when all receipts are
    /// collected.
    fn publish_receipt(&mut self, tx: CompletedTransaction<P, B, S>) {
        // Get or create the pending batch for this transaction.
        let batch_id = tx.batch.checkpoint().index();
        let batch = self.batches.entry(batch_id).or_insert_with(|| PendingBatch::new(tx.batch));

        // Record the receipt and decrement the pending count.
        batch.receipts[tx.index as usize] = Some(tx.receipt);
        batch.pending -= 1;

        // All receipts collected - forward to batch prover.
        if batch.pending == 0 {
            self.outbox.push(self.batches.remove(&batch_id).unwrap());
        }
    }
}
