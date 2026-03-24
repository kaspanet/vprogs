use std::{collections::HashMap, thread::JoinHandle};

use tokio::runtime::Builder;
use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::Inputs as BatchInputs;
use vprogs_zk_transaction_prover::ProvedTransaction;

use crate::{BatchBackend, pending_batch::PendingBatch};

/// Background worker that accumulates proved transaction receipts into per-batch sets, waits
/// for commit ordering, assembles batch witnesses with SMT proofs, and proves them.
pub(crate) struct BatchProverWorker<P: Processor<S>, BB: BatchBackend, S: Store> {
    /// The backend used for journal extraction, image IDs, and batch proving.
    backend: BB,
    /// The store used for reading SMT state proofs at the pre-batch version.
    store: S,
    /// Individual proved transaction receipts from the transaction prover.
    inbox: AsyncQueue<ProvedTransaction<P, BB, S>>,
    /// Output queue for completed batch proof receipts.
    outbox: AsyncQueue<BB::Receipt>,
    /// Receipts accumulating per batch (keyed by batch index).
    pending: HashMap<u64, PendingBatch<P, BB, S>>,
    /// The most recently processed batch (for commit ordering).
    prev_batch: Option<ScheduledBatch<S, P>>,
}

impl<P: Processor<S>, BB: BatchBackend, S: Store> BatchProverWorker<P, BB, S> {
    pub(crate) fn new(
        backend: BB,
        store: S,
        inbox: AsyncQueue<ProvedTransaction<P, BB, S>>,
        outbox: AsyncQueue<BB::Receipt>,
    ) -> Self {
        Self { backend, store, inbox, outbox, pending: HashMap::new(), prev_batch: None }
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
        while !self.inbox.is_singleton() || !self.pending.is_empty() {
            // Drain proved transactions and accumulate receipts per batch.
            while let Some(tx) = self.inbox.pop() {
                if let Some(completed) = self.accumulate(tx) {
                    self.process_batch(completed).await;
                }
            }

            self.inbox.notified().await;
        }
    }

    /// Records a proved transaction receipt. Returns the completed batch when all receipts
    /// for that batch have been collected.
    fn accumulate(&mut self, tx: ProvedTransaction<P, BB, S>) -> Option<PendingBatch<P, BB, S>> {
        let batch_id = tx.batch.checkpoint().index();
        let batch = self.pending.entry(batch_id).or_insert_with(|| PendingBatch::new(tx.batch));

        batch.receipts[tx.index as usize] = Some(tx.receipt);
        batch.pending -= 1;

        if batch.pending == 0 { self.pending.remove(&batch_id) } else { None }
    }

    /// Waits for commit ordering, assembles the batch witness, and proves it.
    async fn process_batch(&mut self, batch: PendingBatch<P, BB, S>) {
        // Wait for the previous batch to commit before reading SMT state.
        if let Some(ref prev) = self.prev_batch {
            prev.wait_committed().await;
        }

        // Skip cancelled batches but still track them for ordering.
        if batch.batch.was_canceled() {
            self.prev_batch = Some(batch.batch);
            return;
        }

        let (scheduled, receipt) = self.assemble_and_prove(batch.batch, batch.receipts).await;
        self.outbox.push(receipt);
        self.prev_batch = Some(scheduled);
    }

    /// Assembles a batch witness from collected transaction receipts, proves the batch, and
    /// returns the scheduled batch handle and raw batch receipt.
    async fn assemble_and_prove(
        &self,
        batch: ScheduledBatch<S, P>,
        receipts: Vec<Option<BB::Receipt>>,
    ) -> (ScheduledBatch<S, P>, BB::Receipt) {
        let prev_version = batch.checkpoint().index().saturating_sub(1);

        // Read resource IDs from the batch's state diffs (one per unique resource).
        let resource_ids: Vec<[u8; 32]> =
            batch.state_diffs().iter().map(|diff| *diff.resource_id().as_bytes()).collect();
        let (proof_bytes, leaf_order) =
            self.store.prove(&resource_ids, prev_version).expect("SMT prove failed");

        // Consume receipt slots -- extract journals by ref, then take ownership.
        let tx_receipts: Vec<BB::Receipt> =
            receipts.into_iter().map(|slot| slot.expect("missing receipt")).collect();
        let journals: Vec<Vec<u8>> = tx_receipts.iter().map(|r| BB::journal_bytes(r)).collect();

        let input =
            BatchInputs::encode(self.backend.image_id(), &proof_bytes, &leaf_order, &journals);
        let receipt = self.backend.prove_batch(&input, tx_receipts).await;
        (batch, receipt)
    }
}
