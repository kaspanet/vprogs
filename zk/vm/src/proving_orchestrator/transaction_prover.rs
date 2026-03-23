use std::{collections::HashMap, pin::Pin, thread::JoinHandle};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_abi::transaction_processor::Inputs;

use super::{
    completed_transaction::CompletedTransaction, pending_batch::PendingBatch,
    pending_transaction::PendingTransaction,
};
use crate::{AsyncQueue, Backend};

/// Worker that dispatches transaction proofs to a [`Backend`] concurrently, tracks
/// per-batch receipt accumulation, and pushes completed batches to the batch prover.
pub(crate) struct TransactionProver<P: Processor<S>, B: Backend, S: Store> {
    /// The proof backend used for proving transactions.
    backend: B,
    /// Proof tasks submitted by execution workers.
    inbox: AsyncQueue<PendingTransaction<P, S>>,
    /// Completed batches forwarded to the batch prover.
    outbox: AsyncQueue<PendingBatch<P, B, S>>,
    /// Receipts accumulating per batch (keyed by batch index).
    pending_batches: HashMap<u64, PendingBatch<P, B, S>>,
    /// Transaction proofs currently in flight.
    #[allow(clippy::type_complexity)]
    pending_transactions: FuturesUnordered<
        Pin<Box<dyn futures::Future<Output = CompletedTransaction<P, B, S>> + Send>>,
    >,
}

impl<P: Processor<S>, B: Backend, S: Store> TransactionProver<P, B, S> {
    pub(crate) fn spawn(
        backend: B,
        inbox: AsyncQueue<PendingTransaction<P, S>>,
        outbox: AsyncQueue<PendingBatch<P, B, S>>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(
                    Self {
                        backend,
                        inbox,
                        outbox,
                        pending_batches: HashMap::new(),
                        pending_transactions: FuturesUnordered::new(),
                    }
                    .run(),
                )
        })
    }

    async fn run(mut self) {
        while !self.should_exit() {
            self.drain_inbox();

            tokio::select! {
                biased;
                () = self.inbox.notified() => {}
                Some(proof) = self.pending_transactions.next(), if !self.pending_transactions.is_empty() => {
                    self.record_receipt(proof);
                }
            }
        }
    }

    /// Drains all queued tasks and submits them to the proof backend.
    fn drain_inbox(&mut self) {
        while let Some(mut task) = self.inbox.pop() {
            if task.batch.was_canceled() {
                continue;
            }

            let Ok(Inputs { tx_index, .. }) = Inputs::decode(&mut task.input_bytes[..]) else {
                continue;
            };

            let proof_future = self.backend.prove_transaction(task.input_bytes);
            let batch = task.batch;

            self.pending_transactions.push(Box::pin(async move {
                CompletedTransaction { batch, index: tx_index, receipt: proof_future.await }
            }));
        }
    }

    /// Records a completed transaction receipt and forwards the batch when all receipts are
    /// collected.
    fn record_receipt(&mut self, proof: CompletedTransaction<P, B, S>) {
        let batch_index = proof.batch.checkpoint().index();
        let tx_count = proof.batch.txs().len() as u32;

        let batch = self.pending_batches.entry(batch_index).or_insert_with(|| PendingBatch {
            batch: proof.batch,
            receipts: vec![None; tx_count as usize],
            received: 0,
        });

        batch.receipts[proof.index as usize] = Some(proof.receipt);
        batch.received += 1;

        // All transactions proven - forward to batch prover.
        if tx_count > 0 && batch.received >= tx_count {
            let batch = self.pending_batches.remove(&batch_index).unwrap();
            self.outbox.push(batch);
        }
    }

    /// Returns true when all external references are dropped and no work remains.
    fn should_exit(&self) -> bool {
        self.inbox.is_singleton() && self.pending_transactions.is_empty()
    }
}
